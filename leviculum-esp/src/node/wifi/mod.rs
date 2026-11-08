//! The WiFi carrier: Reticulum over the ESP32-S3's station radio.
//!
//! LoRa stays interface 0; WiFi adds [`UDP_IFACE`] (1) and [`TCP_IFACE`]
//! (2). A socket task per interface does the wire work — HDLC on both,
//! matching the reference UDPInterface/TCPInterface — and the traffic
//! crosses to the cooperative loop through two channels: [`INBOUND`]
//! carries complete packets up to `node.handle_packet`, [`egress`] drops
//! outbound actions into `udp::TX`/`tcp::TX` for the tasks to frame.
//!
//! Everything below the task boundary is compile-time configuration —
//! the network is a lab fixture, not a runtime setting:
//!
//! | env (at build) | meaning | default |
//! |---|---|---|
//! | `LEVICULUM_WIFI_SSID` | network to join | unset → WiFi dark |
//! | `LEVICULUM_WIFI_PSK` | WPA2 password | empty → open network |
//! | `LEVICULUM_UDP_PORT` | UDP listen and forward port | `4968` |
//! | `LEVICULUM_UDP_FORWARD` | IPv4 datagrams go to | `255.255.255.255` |
//! | `LEVICULUM_TCP_TARGET` | `a.b.c.d:port` to dial | unset → no task |

mod tcp;
mod udp;

use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config, ControllerConfig, WifiController,
};
use leviculum_core::transport::{Action, TickOutput};
use leviculum_core::InterfaceId;
use static_cell::StaticCell;

use crate::node::reticulum::Node;
use crate::UsbLog;

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

const SSID: Option<&'static str> = option_env!("LEVICULUM_WIFI_SSID");
const PSK: &str = match option_env!("LEVICULUM_WIFI_PSK") {
    Some(s) => s,
    None => "",
};
const TCP_TARGET: Option<&'static str> = option_env!("LEVICULUM_TCP_TARGET");

/// Bring the radio up and spawn the socket tasks. Returns whether the
/// WiFi interfaces registered — false only when no SSID was compiled in
/// or the controller refused its configuration; the radio itself
/// (join, DHCP) is async and logged from the tasks.
pub fn start(
    spawner: Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
    seed: u64,
    log: &mut UsbLog<'static>,
) -> bool {
    let Some(ssid) = SSID else {
        log.line("[WIFI] ", format_args!("state=off reason=no-ssid"));
        return false;
    };

    let auth = if PSK.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(PSK.try_into().expect("PSK <= 64 chars"))
    };
    let station = Config::Station(
        StationConfig::default()
            .with_ssid(ssid.try_into().expect("SSID <= 32 chars"))
            .with_authentication(auth),
    );

    let interface = esp_radio::wifi::Interface::station();
    let controller = match WifiController::new(
        wifi,
        ControllerConfig::default().with_initial_config(station),
    ) {
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
    if TCP_TARGET.is_some() {
        spawner.spawn(tcp::task(stack).unwrap());
    }
    log.line("[WIFI] ", format_args!("state=up ssid={ssid}"));
    true
}

static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();

/// Name the WiFi interfaces and set their MTUs (the loop calls this only
/// when [`start`] returned true).
pub fn register_interfaces(node: &mut Node) {
    node.set_interface_name(UDP_IFACE.0, "wifi_udp".into());
    node.set_interface_hw_mtu(UDP_IFACE.0, UDP_HW_MTU as u32);
    node.set_interface_name(TCP_IFACE.0, "wifi_tcp".into());
    node.set_interface_hw_mtu(TCP_IFACE.0, TCP_HW_MTU as u32);
}

/// Route a `TickOutput`'s actions: WiFi-bound traffic into the socket
/// tasks' channels, the rest back for the LoRa carrier. Broadcast goes
/// to every medium the exclusion does not name.
pub fn egress(mut out: TickOutput) -> TickOutput {
    let actions = core::mem::take(&mut out.actions);
    for action in actions {
        match action {
            Action::SendPacket {
                iface, ref data, ..
            } if iface == UDP_IFACE => {
                log_str(alloc::format!("tx=udp len={}", data.len()));
                let _ = udp::TX.try_send(frame(data));
            }
            Action::SendPacket {
                iface, ref data, ..
            } if iface == TCP_IFACE => {
                log_str(alloc::format!("tx=tcp len={}", data.len()));
                let _ = tcp::TX.try_send(frame(data));
            }
            Action::SendPacket { .. } => out.actions.push(action),
            Action::Broadcast {
                ref data,
                exclude_iface,
                ..
            } => {
                if exclude_iface != Some(UDP_IFACE) {
                    let _ = udp::TX.try_send(frame(data));
                }
                if exclude_iface != Some(TCP_IFACE) {
                    let _ = tcp::TX.try_send(frame(data));
                }
                if exclude_iface != Some(InterfaceId(0)) {
                    out.actions.push(action);
                }
            }
        }
    }
    out
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
/// few sites that use it (bring-up, once per DHCP).
pub fn log_str(text: alloc::string::String) {
    let _ = LOG.try_send(alloc::boxed::Box::leak(text.into_boxed_str()));
}

/// `a.b.c.d` → IPv4 octets, or `None`. Runtime parse — the env values are
/// text, the socket wants numbers.
pub fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut i = 0;
    for part in text.split('.') {
        if i == 4 {
            return None;
        }
        out[i] = part.parse().ok()?;
        i += 1;
    }
    (i == 4).then_some(out)
}

/// `a.b.c.d:port` → an endpoint, or `None`.
pub fn parse_endpoint(text: &str) -> Option<([u8; 4], u16)> {
    let (addr, port) = text.split_once(':')?;
    Some((parse_ipv4(addr)?, port.parse().ok()?))
}

/// The WiFi controller's own task: join the configured network, re-join
/// when the AP drops us. Never returns.
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                let _ = LOG.try_send("sta=connected");
                let _ = controller.wait_for_disconnect_async().await;
                let _ = LOG.try_send("sta=disconnected");
            }
            Err(_) => {
                let _ = LOG.try_send("sta=connect-failed");
            }
        }
        embassy_time::Timer::after_millis(5000).await;
    }
}

/// The embassy-net runner: pumps the driver and the stack. Never returns.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await;
}
