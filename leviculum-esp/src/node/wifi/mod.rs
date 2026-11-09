//! The WiFi carrier: Reticulum over the ESP32-S3's station radio.
//!
//! LoRa stays interface 0; WiFi adds [`UDP_IFACE`] (1) and [`TCP_IFACE`]
//! (2). A socket task per interface does the wire work — HDLC on both,
//! matching the reference UDPInterface/TCPInterface — and the traffic
//! crosses to the cooperative loop through two channels: [`INBOUND`]
//! carries complete packets up to `node.handle_packet`, [`egress`] drops
//! outbound actions into `udp::TX`/`tcp::TX` for the tasks to frame.
//!
//! What the radio joins and how the sockets are wired is not compiled
//! in anymore: [`settings`] holds the running values, seeded from the
//! build-time env knobs on a fresh flash and overridable on the debug
//! port (`WIFI SSID`, `WIFI PSK`, `WIFI UDP`, `WIFI TCP`, `WIFI SAVE`,
//! `WIFI RESET` — see [`on_line`]). Each task re-reads the settings on
//! every cycle and reacts: the connection task re-applies the station
//! config, the TCP task re-dials a new target, the UDP task rebinds a
//! changed port. Nothing needs a rebuild.

pub(crate) mod routing;
pub(crate) mod settings;
mod tcp;
mod udp;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config, ControllerConfig, WifiController,
};
use leviculum_core::transport::{Action, TickOutput};
use leviculum_core::InterfaceId;
use static_cell::StaticCell;

use crate::node::reticulum::Node;
use crate::node::storage::{self, SharedFlash};
use crate::store;
use crate::UsbLog;

pub(crate) use settings::{WifiCmd, WifiSettings};

/// The UDP interface's Reticulum id. LoRa is 0.
pub const UDP_IFACE: InterfaceId = InterfaceId(1);
/// The TCP client interface's Reticulum id.
pub const TCP_IFACE: InterfaceId = InterfaceId(2);

/// TCP HW_MTU. The value is a policy (announce sizing), not a buffer
/// bound — TCP tolerates what LoRa cannot — kept at the figure the
/// host-side TCP interfaces negotiate below.
pub const TCP_HW_MTU: usize = 1500;
/// UDP HW_MTU: one datagram, comfortably inside a 1500-byte MTU.
pub const UDP_HW_MTU: usize = 1400;

/// Complete Reticulum packets lifted off a socket, `(interface, bytes)`,
/// for the main loop's `node.handle_packet`.
pub static INBOUND: Channel<CriticalSectionRawMutex, (InterfaceId, Vec<u8>), 8> = Channel::new();
/// One-line events the tasks can't print themselves (the log lives in the
/// loop). Drained into `[WIFI]` lines each pass.
pub static LOG: Channel<CriticalSectionRawMutex, &'static str, 8> = Channel::new();

/// The running settings. Seeded in [`start`] from flash (or the env
/// defaults) and mutated by [`on_line`]; every task re-reads it each
/// cycle, so a change is picked up without any cross-task signalling.
/// `Option`: a const-constructible placeholder until `start` fills it —
/// no task runs before that.
static SETTINGS: Mutex<CriticalSectionRawMutex, core::cell::RefCell<Option<WifiSettings>>> =
    Mutex::new(core::cell::RefCell::new(None));
/// Bumped on every settings change, so a task can cheaply detect "re-read
/// the settings" without taking the mutex on each pass.
static VERSION: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn cfg_init(initial: WifiSettings) {
    SETTINGS.lock(|cell| *cell.borrow_mut() = Some(initial));
}

/// A snapshot of the running settings.
fn cfg_current() -> WifiSettings {
    SETTINGS.lock(|cell| {
        cell.borrow()
            .clone()
            .expect("settings seeded before any task runs")
    })
}

/// Mutate the running settings and bump the version.
fn cfg_update(f: impl FnOnce(&mut WifiSettings)) {
    SETTINGS.lock(|cell| {
        let mut slot = cell.borrow_mut();
        f(slot.as_mut().expect("settings seeded before any task runs"))
    });
    VERSION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// The settings-generation counter: tasks compare this against their last
/// read to know whether to re-read (and re-apply) the settings.
pub(crate) fn cfg_version() -> u64 {
    VERSION.load(core::sync::atomic::Ordering::Relaxed) as u64
}

/// Bring the radio up and spawn the socket tasks. Returns whether the
/// WiFi interfaces registered (false only when the controller refused
/// its configuration); the radio itself (join, DHCP) is async and logged
/// from the tasks.
///
/// The settings come from the flash slot when it holds a valid record,
/// otherwise from the compiled env defaults — the same values the
/// previous const-based build ran on.
pub fn start(
    spawner: Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
    seed: u64,
    flash: &mut esp_storage::FlashStorage<'static>,
    log: &mut UsbLog<'static>,
) -> bool {
    let stored = store::load_wifi_settings(flash);
    let initial = stored.clone().unwrap_or_else(WifiSettings::env_defaults);
    let src = if stored.is_some() {
        "stored"
    } else {
        "default"
    };
    cfg_init(initial.clone());

    let station = match initial.ssid.as_deref() {
        Some(ssid) => match station_config(ssid, &initial.psk) {
            Ok(cfg) => Some(cfg),
            Err(why) => {
                log.line("[WIFI] ", format_args!("state=down reason={why}"));
                return false;
            }
        },
        None => None,
    };

    let interface = esp_radio::wifi::Interface::station();
    let config = match station {
        Some(cfg) => ControllerConfig::default().with_initial_config(Config::Station(cfg)),
        None => ControllerConfig::default(),
    };
    let controller = match WifiController::new(wifi, config) {
        Ok(c) => c,
        Err(_) => {
            log.line("[WIFI] ", format_args!("state=down reason=init"));
            return false;
        }
    };

    let (stack, runner) = embassy_net::new(
        interface,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::<8>::new()),
        seed,
    );

    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(udp::task(stack).unwrap());
    spawner.spawn(tcp::task(stack).unwrap());
    match initial.ssid.as_deref() {
        Some(ssid) => log.line("[WIFI] ", format_args!("state=up ssid={ssid} cfg={src}")),
        None => log.line("[WIFI] ", format_args!("state=dark ssid=none cfg={src}")),
    }
    true
}

/// One station config from the settings' ssid + psk. Shared by boot and
/// by the connection task's re-apply.
fn station_config(ssid: &str, psk: &str) -> Result<StationConfig, &'static str> {
    let sid = ssid.try_into().map_err(|_| "bad-ssid")?;
    let auth = if psk.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(psk.try_into().map_err(|_| "bad-psk")?)
    };
    Ok(StationConfig::default()
        .with_ssid(sid)
        .with_authentication(auth))
}

static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();

/// Name the WiFi interfaces and set their MTUs.
pub fn register_interfaces(node: &mut Node) {
    node.set_interface_name(UDP_IFACE.0, "wifi_udp".into());
    node.set_interface_hw_mtu(UDP_IFACE.0, UDP_HW_MTU as u32);
    node.set_interface_name(TCP_IFACE.0, "wifi_tcp".into());
    node.set_interface_hw_mtu(TCP_IFACE.0, TCP_HW_MTU as u32);
}

/// Route a `TickOutput`'s actions: WiFi-bound traffic into the socket
/// tasks' channels, the rest back for the LoRa carrier. Broadcast goes
/// to every medium the exclusion does not name.
///
/// The per-action decision is the pure matrix in [`routing`] — this
/// function only carries it to the channels.
pub fn egress(mut out: TickOutput) -> TickOutput {
    let actions = core::mem::take(&mut out.actions);
    for action in actions {
        let route = match &action {
            Action::SendPacket { iface, .. } => routing::route(routing::Packet::Send {
                iface: iface.0 as u8,
            }),
            Action::Broadcast { exclude_iface, .. } => routing::route(routing::Packet::Broadcast {
                exclude: exclude_iface.map(|i| i.0 as u8),
            }),
        };
        let data = match &action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
        };
        if route.udp {
            log_str(alloc::format!("tx=udp len={}", data.len()));
            let _ = udp::TX.try_send(frame(data));
        }
        if route.tcp {
            log_str(alloc::format!("tx=tcp len={}", data.len()));
            let _ = tcp::TX.try_send(frame(data));
        }
        if route.lora {
            out.actions.push(action);
        }
    }
    out
}

/// A debug-port line that starts with `WIFI`: parse it, apply it to the
/// running settings, and log the outcome. Returns whether the line was
/// consumed (it is not for the app).
pub fn on_line(line: &str, log: &mut UsbLog<'static>, flash: &'static SharedFlash) -> bool {
    let Some(result) = settings::parse_command(line) else {
        return false;
    };
    let Err(why) = result else {
        // Parsed; fall through to apply.
        let cmd = result.unwrap();
        apply_command(cmd, log, flash);
        return true;
    };
    log.line("[WIFI] ", format_args!("error: {why}"));
    true
}

fn apply_command(cmd: WifiCmd, log: &mut UsbLog<'static>, flash: &'static SharedFlash) {
    match cmd {
        WifiCmd::Status => {
            let s = cfg_current();
            let ssid = s.ssid.as_deref().unwrap_or("none");
            let psk_len = s.psk.len();
            let fwd = s
                .udp_forward
                .map(format_ipv4)
                .unwrap_or_else(|| "off".to_string());
            let tcp = s
                .tcp_target
                .map(|(a, p)| format!("{}:{p}", format_ipv4(a)))
                .unwrap_or_else(|| "off".to_string());
            log.line(
                "[WIFI] ",
                format_args!(
                    "status ssid={ssid} psk={psk_len} udp={udp} fwd={fwd} tcp={tcp}",
                    udp = s.udp_port,
                ),
            );
        }
        WifiCmd::SetSsid(ssid) => {
            cfg_update(|s| s.ssid = ssid.clone());
            log.line(
                "[WIFI] ",
                format_args!(
                    "set ssid={} (save to persist)",
                    ssid.as_deref().unwrap_or("off")
                ),
            );
        }
        WifiCmd::SetPsk(psk) => {
            cfg_update(|s| s.psk = psk.clone());
            log.line(
                "[WIFI] ",
                format_args!("set psk={} chars (save to persist)", psk.len()),
            );
        }
        WifiCmd::SetUdp { port, forward } => {
            cfg_update(|s| {
                s.udp_port = port;
                s.udp_forward = forward;
            });
            let fwd = forward
                .map(format_ipv4)
                .unwrap_or_else(|| "off".to_string());
            log.line(
                "[WIFI] ",
                format_args!("set udp={port} fwd={fwd} (save to persist)"),
            );
        }
        WifiCmd::SetTcp(target) => {
            cfg_update(|s| s.tcp_target = target);
            let tcp = target
                .map(|(a, p)| format!("{}:{p}", format_ipv4(a)))
                .unwrap_or_else(|| "off".to_string());
            log.line("[WIFI] ", format_args!("set tcp={tcp} (save to persist)"));
        }
        WifiCmd::Save => {
            let s = cfg_current();
            let saved = storage::with(flash, |f| store::save_wifi_settings(f, &s)).is_ok();
            log.line(
                "[WIFI] ",
                if saved {
                    format_args!("saved")
                } else {
                    format_args!("save FAILED")
                },
            );
        }
        WifiCmd::Reset => {
            cfg_update(|s| *s = WifiSettings::env_defaults());
            log.line(
                "[WIFI] ",
                format_args!("reset to compiled defaults (save to persist)"),
            );
        }
    }
}

/// `a.b.c.d` from octets.
fn format_ipv4(a: [u8; 4]) -> String {
    alloc::format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// HDLC-frame one packet into a fresh `Vec` (`FLAG + escaped + FLAG`,
/// the simplified no-CRC framing TCP and UDP interfaces share).
fn frame(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    leviculum_core::framing::hdlc::frame(data, &mut out);
    out
}

/// Lift complete packets out of a deframer result batch into `INBOUND`.
fn publish(iface: InterfaceId, results: Vec<leviculum_core::framing::hdlc::DeframeResult>) {
    use leviculum_core::framing::hdlc::DeframeResult;
    for r in results {
        if let DeframeResult::Frame(packet) = r {
            let _ = INBOUND.try_send((iface, packet));
        }
    }
}

/// Log a runtime-built line on the loop's `[WIFI]` channel. The channel
/// carries `&'static str`, so a formed string is leaked — bounded to the
/// few sites that use it (bring-up, config changes, once per DHCP).
pub fn log_str(text: alloc::string::String) {
    let _ = LOG.try_send(alloc::boxed::Box::leak(text.into_boxed_str()));
}

/// The WiFi controller's own task: keep the station on the settings'
/// network, re-applying the config and re-joining whenever the settings
/// change or the AP drops us. Never returns.
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    // The (ssid, psk) pair last handed to the driver. A settings change
    // is detected by comparing against this before every connect.
    let mut applied: Option<(String, String)> = None;
    loop {
        let s = cfg_current();
        let desired = s.ssid.as_ref().map(|ssid| (ssid.clone(), s.psk.clone()));
        let Some((ssid, psk)) = desired else {
            // No SSID configured: keep the radio dark and wait.
            let _ = controller.disconnect_async().await;
            applied = None;
            embassy_time::Timer::after_millis(1000).await;
            continue;
        };
        if applied.as_ref() != Some(&(ssid.clone(), psk.clone())) {
            // Settings changed since last time: drop the old link, tell
            // the driver the new network, then connect to it.
            let _ = controller.disconnect_async().await;
            match station_config(&ssid, &psk) {
                Ok(cfg) => match controller.set_config(&Config::Station(cfg)) {
                    Ok(()) => applied = Some((ssid.clone(), psk.clone())),
                    Err(_) => {
                        log_str("sta=config-rejected".into());
                        embassy_time::Timer::after_millis(5000).await;
                        continue;
                    }
                },
                Err(why) => {
                    log_str(alloc::format!("sta={why}"));
                    embassy_time::Timer::after_millis(5000).await;
                    continue;
                }
            }
        }
        let version = cfg_version();
        match controller.connect_async().await {
            Ok(_) => {
                let _ = LOG.try_send("sta=connected");
                // Hold the link until the AP drops us or the settings
                // change under us (then reconnect to the new target).
                loop {
                    if cfg_version() != version {
                        break;
                    }
                    match embassy_futures::select::select(
                        controller.wait_for_disconnect_async(),
                        embassy_time::Timer::after_millis(1000),
                    )
                    .await
                    {
                        embassy_futures::select::Either::First(_) => break,
                        embassy_futures::select::Either::Second(_) => {}
                    }
                }
            }
            Err(_) => {
                let _ = LOG.try_send("sta=connect-failed");
                embassy_time::Timer::after_millis(5000).await;
            }
        }
    }
}

/// The embassy-net runner: pumps the driver and the stack. Never returns.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await;
}
