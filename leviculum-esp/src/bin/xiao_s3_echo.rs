//! Firmware: `leviculum-automation` Echo vozlišče za Seeed XIAO ESP32-S3 +
//! Wio-SX1262 kit (`xiao_s3`).
//!
//! Najmanjši možen automation-device firmware na ESP32-S3: `App<Echo>` + `NoIo`.
//! Oddaljeno vozlišče pošlje `ECHC` params okvir (do 32 bajtov) v podpisanem
//! opportunističnem LXMF sporočilu → vozlišče preveri podpis in shrani podatke →
//! ob naslednjem `tick` vrne `ECHS` report z istimi bajti nazaj pošiljatelju.
//!
//! Gradnja:
//!   source ~/export-esp.sh
//!   cd leviculum-esp && cargo build --release --bin xiao_s3_echo

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    main,
    rng::Trng,
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
};
use leviculum_automation::{
    echo::{Echo, EchoReport},
    hal::ConfigStore,
    App, Device, Inbound, NoIo,
};
use leviculum_core::{
    node::{NodeCoreBuilder, NodeEvent, SendError},
    traits::Storage,
    transport::{Action, TickOutput},
    DestinationHash, Identity, InterfaceId, InterfaceMode, MemoryStorage,
};
use leviculum_esp::{
    automation::UptimeClock,
    boards::xiao_s3,
    radio::{Radio, Rx},
    store,
    sx1262::Sx1262Bus,
    UsbLog,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// Radijski profil za hitro lokalno stacionarno mrežo (`/etc/reticulum/config`):
/// 869.463 MHz, SF7, BW 250 kHz (0x05), CR 4/5 (0x01), 22 dBm.
const LORA_FREQ: u32 = 869_463_000;
const LORA_SF: u8 = 7;
const LORA_BW: u8 = 0x05; // 250 kHz (SX1262 Table 14-47)
const LORA_BW_HZ: u32 = 250_000;
const LORA_CR: u8 = 0x01; // CR 4/5
const LORA_CR_DENOM: u8 = 5;
const LORA_TX_POWER_DBM: i8 = 22;
const LORA_PREAMBLE: u16 = 18;

const ANNOUNCE_MS: u64 = 60_000;

struct EspClock;
impl leviculum_core::traits::Clock for EspClock {
    fn now_ms(&self) -> u64 {
        leviculum_esp::uptime_ms()
    }
}

/// RAM-only hramba za `Echo` parametre (`ECHC` je podatek za odmev, ne
/// trajna konfiguracija, zato ne obrabljamo flash pomnilnika).
struct RamStore(Option<Vec<u8>>);

impl ConfigStore for RamStore {
    fn load(&mut self) -> Option<Vec<u8>> {
        self.0.clone()
    }

    fn save(&mut self, blob: &[u8]) -> bool {
        self.0 = Some(blob.to_vec());
        true
    }
}

#[main]
fn main() -> ! {
    let peripherals = leviculum_esp::init();
    let mut log = UsbLog::new(peripherals.USB_DEVICE);
    log.identify(&xiao_s3::CONFIG);
    let mut led = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());

    // ---- 1. identiteta + flash ----------------------------------------
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let _trng_src = esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let identity = match store::load_identity(&mut flash)
        .and_then(|k| Identity::from_private_key_bytes(&k).ok())
    {
        Some(id) => {
            log.line("[ECHO] ", format_args!("identity=restored"));
            id
        }
        None => {
            let mut trng = Trng::try_new().expect("TRNG source held");
            let id = Identity::generate(&mut trng);
            let persisted = id
                .private_key_bytes()
                .ok()
                .and_then(|k| store::save_identity(&mut flash, &k).ok())
                .is_some();
            log.line(
                "[ECHO] ",
                format_args!("identity=generated persisted={persisted}"),
            );
            id
        }
    };

    let our_dest = leviculum_automation::lxmf::delivery_hash(&identity).unwrap_or([0; 16]);
    {
        let mut hex = [0u8; 32];
        to_hex(&our_dest, &mut hex);
        let hex_str = core::str::from_utf8(&hex).unwrap_or("");
        log.line("[ECHO] ", format_args!("delivery_hash={hex_str}"));
        log.line("[AUTO] ", format_args!("dest={hex_str}"));
    }

    // ---- 2. SX1262 LoRa radio -----------------------------------------
    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(xiao_s3::LORA_SPI_FREQ_HZ))
            .with_mode(Mode::_0),
    )
    .expect("spi config")
    .with_sck(peripherals.GPIO7)
    .with_mosi(peripherals.GPIO9)
    .with_miso(peripherals.GPIO8)
    .into_async();
    let bus = Sx1262Bus::new(
        spi,
        Output::new(peripherals.GPIO41, Level::High, OutputConfig::default()),
        Input::new(
            peripherals.GPIO40,
            InputConfig::default().with_pull(Pull::None),
        ),
    );
    let mut radio = Radio::new(
        bus,
        Output::new(peripherals.GPIO42, Level::High, OutputConfig::default()),
        Input::new(
            peripherals.GPIO39,
            InputConfig::default().with_pull(Pull::None),
        ),
        Output::new(peripherals.GPIO38, Level::Low, OutputConfig::default()),
    );
    let radio_ok = radio.init(LORA_FREQ).is_ok()
        && radio
            .configure(
                LORA_FREQ,
                LORA_SF,
                LORA_BW,
                LORA_CR,
                LORA_TX_POWER_DBM,
                LORA_PREAMBLE,
            )
            .is_ok()
        && radio.listen().is_ok();
    log.line(
        "[LORA] ",
        format_args!(
            "state={} freq={} sf={} bw_hz={} cr=4/{} txdBm={}",
            if radio_ok { "up" } else { "down" },
            LORA_FREQ,
            LORA_SF,
            LORA_BW_HZ,
            LORA_CR_DENOM,
            LORA_TX_POWER_DBM,
        ),
    );

    // ---- 3. Reticulum NodeCore + lxmf.delivery ------------------------
    let mut node = NodeCoreBuilder::new()
        .identity(identity.clone())
        .enable_transport(true)
        .respond_to_probes(true)
        .build_boxed(
            Trng::try_new().expect("TRNG source held"),
            EspClock,
            MemoryStorage::compact(),
        );
    node.set_interface_name(0, "lora_sx1262".into());
    node.set_interface_hw_mtu(0, 508);
    node.set_interface_mode(0, InterfaceMode::Gateway);
    if let Ok(d) = leviculum_lxmf::LxmfNode::delivery_destination_without_inbox(identity.clone()) {
        node.register_destination(d);
    }

    let mut tx_seq = 0u8;
    let mut next_announce = 0u64;
    let mut clock_seeded = false;
    let mut reassembler = leviculum_core::rnode::SplitReassembler::new();
    let mut rx_tick = 0u32;
    let mut lora_buf = [0u8; 256];

    // ---- 4. App<Echo> -------------------------------------------------
    let clock = UptimeClock;
    let mut cfg_store = RamStore(None);
    let mut app = App::boot(Echo::new(), &mut cfg_store, clock_now(), &mut log);
    let mut pending_report: Option<(EchoReport, bool)> = None;

    let mut rx_line = [0u8; 1100];
    let mut rx_len = 0usize;
    let delay = Delay::new();

    loop {
        // ---- LoRa sprejem ---------------------------------------------
        if radio_ok {
            loop {
                match radio.poll_rx(&mut lora_buf) {
                    Ok(Rx::Frame { len, status }) => {
                        log.line(
                            "[LORA] ",
                            format_args!("rx len={} rssi={} snr={}", len, status.rssi, status.snr),
                        );
                        if let Some(packet) = reassembler.feed(&lora_buf[..len as usize], rx_tick) {
                            let out = node.handle_packet(InterfaceId(0), &packet);
                            let mut announce_now = false;
                            for ev in &out.events {
                                match ev {
                                    NodeEvent::AnnounceReceived { announce, .. } => {
                                        let dh = announce.destination_hash().as_bytes();
                                        log.line(
                                            "[ANNOUNCE] ",
                                            format_args!(
                                                "heard dest={:02x}{:02x}{:02x}{:02x}",
                                                dh[0], dh[1], dh[2], dh[3]
                                            ),
                                        );
                                        if !clock_seeded && node.has_plausible_wall_clock() {
                                            clock_seeded = true;
                                            announce_now = true;
                                        }
                                    }
                                    NodeEvent::PacketReceived {
                                        destination, data, ..
                                    } if destination.as_bytes() == &our_dest => {
                                        let signer = leviculum_automation::lxmf::peek_source(data)
                                            .and_then(|src| {
                                                node.storage().get_identity(&src).cloned()
                                            });
                                        match app.on_message(
                                            data,
                                            &our_dest,
                                            clock_now(),
                                            &mut log,
                                            signer.as_ref(),
                                        ) {
                                            Inbound::Applied { blob, .. } => {
                                                app.device_mut()
                                                    .set_last_signal(status.rssi, status.snr);
                                                cfg_store.save(&blob);
                                            }
                                            Inbound::Rejected => {
                                                log.line(
                                                    "[ECHO] ",
                                                    format_args!("params=rejected"),
                                                );
                                            }
                                            Inbound::Ignored => {}
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            dispatch(out, &mut radio, &mut log, &mut tx_seq);
                            if announce_now {
                                next_announce = 0;
                            }
                        }
                        rx_tick = rx_tick.wrapping_add(1);
                    }
                    Ok(Rx::BadCrc) => {
                        log.line("[LORA] ", format_args!("rx err=crc"));
                        break;
                    }
                    Ok(Rx::Idle) => break,
                    Err(_) => {
                        let _ = radio.listen();
                        break;
                    }
                }
            }

            dispatch(node.handle_timeout(), &mut radio, &mut log, &mut tx_seq);

            if clock_now() >= next_announce {
                if let Ok(out) = node.announce_destination(
                    &DestinationHash::new(our_dest),
                    Some(b"leviculum-xiao_s3-echo"),
                ) {
                    dispatch(out, &mut radio, &mut log, &mut tx_seq);
                    let mut hex = [0u8; 32];
                    to_hex(&our_dest, &mut hex);
                    log.line(
                        "[ECHO] ",
                        format_args!(
                            "announced delivery_hash={}",
                            core::str::from_utf8(&hex).unwrap_or("")
                        ),
                    );
                }
                next_announce = clock_now() + ANNOUNCE_MS;
            }
        }

        // ---- Echo tick + odmev ----------------------------------------
        let tick = app.tick(&mut NoIo, &clock, &mut log);
        if let Some(report) = tick.report {
            pending_report = Some((report, false));
            led.toggle();
        }

        if let Some((report, ref mut path_requested)) = pending_report {
            if let Some(target) = app.target() {
                if let Some(on_air) = app.report_message(&identity, &our_dest, &report, clock_now())
                {
                    if radio_ok {
                        let target_hash = DestinationHash::new(target);
                        match node.send_single_packet(&target_hash, &on_air) {
                            Ok((_, out)) => {
                                dispatch(out, &mut radio, &mut log, &mut tx_seq);
                                log.line(
                                    "[ECHO] ",
                                    format_args!(
                                        "reply sent dest={:02x}{:02x}{:02x}{:02x}",
                                        target[0], target[1], target[2], target[3]
                                    ),
                                );
                                pending_report = None;
                            }
                            Err(SendError::PacingDelay { .. }) => {}
                            Err(_) => {
                                if !*path_requested {
                                    *path_requested = true;
                                    log.line(
                                        "[ECHO] ",
                                        format_args!(
                                            "reply deferred (requesting path) dest={:02x}{:02x}{:02x}{:02x}",
                                            target[0], target[1], target[2], target[3]
                                        ),
                                    );
                                    let _ = node.remove_path(&target);
                                    let out = node.request_path(&target_hash);
                                    dispatch(out, &mut radio, &mut log, &mut tx_seq);
                                }
                            }
                        }
                    } else {
                        pending_report = None;
                    }
                }
            }
        }

        // ---- Serijski (USB) ukazi za testiranje na mizi ---------------
        let mut rx = [0u8; 64];
        let n = log.read_bytes(&mut rx);
        for &b in &rx[..n] {
            if b == b'\n' || b == b'\r' {
                if rx_len > 0 {
                    let line = core::str::from_utf8(&rx_line[..rx_len]).unwrap_or("");
                    handle_line(
                        line.trim(),
                        &mut app,
                        &mut cfg_store,
                        &our_dest,
                        &mut next_announce,
                        &mut log,
                    );
                    rx_len = 0;
                }
            } else if rx_len < rx_line.len() {
                rx_line[rx_len] = b;
                rx_len += 1;
            } else {
                rx_len = 0;
            }
        }

        delay.delay_millis(5);
    }
}

fn clock_now() -> u64 {
    leviculum_esp::uptime_ms()
}

/// Serijski ukazi na USB konzoli:
/// - `AUTO TARGET <32hex>` — ročno nastavi ponor za poročila
/// - `AUTO LXMF <hex>` — vbrizga opportunistično LXMF sporočilo prek USB
/// - `AUTO GET` — izpiše trenutno shranjeno vsebino v `Echo`
/// - `AUTO ANNOUNCE` — takoj sproži Reticulum najavo (`ANNOUNCE`)
fn handle_line<S: ConfigStore>(
    line: &str,
    app: &mut App<Echo>,
    store: &mut S,
    our_dest: &[u8; 16],
    next_announce: &mut u64,
    log: &mut UsbLog<'_>,
) {
    let mut it = line.split_whitespace();
    if it.next() != Some("AUTO") {
        return;
    }
    match it.next() {
        Some("TARGET") => {
            let mut t = [0u8; 16];
            if unhex(it.next().unwrap_or(""), &mut t) == 16 {
                let blob = app.set_target(t);
                let ok = store.save(&blob);
                log.line(
                    "[ECHO] ",
                    format_args!("cmd=target result=ok persisted={ok}"),
                );
            } else {
                log.line("[ECHO] ", format_args!("cmd=target result=bad-hash"));
            }
        }
        Some("LXMF") => {
            let mut data = [0u8; 512];
            let n = unhex(it.next().unwrap_or(""), &mut data);
            let r = match app.on_message(&data[..n], our_dest, clock_now(), log, None) {
                Inbound::Applied { blob, .. } => {
                    app.device_mut().set_last_signal(i16::MIN, i16::MIN);
                    if store.save(&blob) {
                        "ok"
                    } else {
                        "ok-not-persisted"
                    }
                }
                Inbound::Rejected => "rejected",
                Inbound::Ignored => "not-lxmf",
            };
            log.line("[ECHO] ", format_args!("cmd=lxmf result={r}"));
        }
        Some("GET") => {
            let mut hex = [0u8; 32];
            to_hex(our_dest, &mut hex);
            let r = app.device().report(clock_now());
            log.line(
                "[ECHO] ",
                format_args!(
                    "delivery_hash={} len={} applied={} rssi={} snr={}",
                    core::str::from_utf8(&hex).unwrap_or(""),
                    r.len,
                    r.applied,
                    r.rssi_dbm,
                    r.snr_db,
                ),
            );
        }
        Some("ANNOUNCE") => {
            *next_announce = 0;
            log.line("[ECHO] ", format_args!("cmd=announce scheduled=now"));
        }
        _ => {}
    }
}

/// Pretvori `TickOutput` akcije v RNode LoRa okvirje in jih odda prek SX1262.
fn dispatch(
    out: TickOutput,
    radio: &mut Radio<'_, esp_hal::spi::master::Spi<'static, esp_hal::Async>>,
    log: &mut UsbLog<'_>,
    tx_seq: &mut u8,
) {
    for action in out.actions {
        let data = match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
        };
        for frame in leviculum_core::rnode::build_lora_frames(&data, *tx_seq << 4) {
            let timeout = leviculum_core::sx126x::tx_timeout_ms(
                frame.len() as u32,
                LORA_BW_HZ,
                LORA_SF,
                LORA_CR_DENOM,
                LORA_PREAMBLE,
            );
            match radio.transmit(&frame, timeout) {
                Ok(()) => log.line("[LORA] ", format_args!("tx len={}", frame.len())),
                Err(_) => log.line("[LORA] ", format_args!("tx err=timeout")),
            }
        }
        *tx_seq = tx_seq.wrapping_add(1) & 0x0F;
        let _ = radio.listen();
    }
}

fn unhex(s: &str, out: &mut [u8]) -> usize {
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut n = 0;
    while 2 * n + 1 < b.len() && n < out.len() {
        match (nib(b[2 * n]), nib(b[2 * n + 1])) {
            (Some(h), Some(l)) => out[n] = (h << 4) | l,
            _ => return 0,
        }
        n += 1;
    }
    n
}

fn to_hex(bytes: &[u8], out: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut n = 0;
    for &b in bytes {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = HEX[(b >> 4) as usize];
        out[n + 1] = HEX[(b & 0xf) as usize];
        n += 2;
    }
    n
}
