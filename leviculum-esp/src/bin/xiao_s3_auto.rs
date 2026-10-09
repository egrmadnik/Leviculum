//! Automation-device wrapper for the Seeed XIAO ESP32-S3 + Wio-SX1262
//! kit: the `leviculum_automation` thermostat on this board.
//!
//! This file is the template. Everything in it is one of four things:
//!
//! 1. **Pins → HAL adaptors** (`automation.rs`): DS18B20 on D1 as the
//!    sensor, D3 as the actuator, a flash sector as the store.
//! 2. **Identity + mesh**: load/generate the node identity, bring the
//!    SX1262 up, box a `NodeCore`, register `lxmf.delivery`, announce.
//! 3. **The loop**: `app.tick` → actuate / report; radio poll →
//!    `handle_packet` → `app.on_message`; `TickOutput` → air.
//! 4. **Bench path**: `AUTO TARGET <32hex>`, `AUTO GET` on the debug
//!    port.
//!
//! To make a different device, change the one `Thermostat::new()` and
//! whichever adaptor its input/output needs. To put it on the nRF kit,
//! swap the adaptors and the radio/node glue for `leviculum-nrf`'s —
//! the loop body is the same calls.

#![no_std]
#![no_main]

extern crate alloc;

use esp_hal::gpio::{DriveMode, Flex};
use esp_hal::rng::Trng;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    main,
    spi::{
        master::{Config as SpiConfig, Spi},
        Mode,
    },
    time::Rate,
};
use leviculum_automation::{hal::ConfigStore, thermostat::Thermostat, App, Device, Inbound};
use leviculum_core::{
    node::{NodeCoreBuilder, NodeEvent},
    transport::{Action, TickOutput},
    DestinationHash, Identity, InterfaceId, MemoryStorage,
};
use leviculum_esp::{
    automation::{Ds18b20Sensor, FlashStore, GpioActuator, UptimeClock},
    boards::xiao_s3,
    ds18b20,
    radio::{Radio, Rx},
    store,
    sx1262::Sx1262Bus,
    UsbLog,
};

esp_bootloader_esp_idf::esp_app_desc!();

const LORA_FREQ: u32 = 869_463_000;
const LORA_SF: u8 = 8;
const LORA_BW: u8 = 0x04;
const LORA_CR: u8 = 0x01;
const LORA_PREAMBLE: u16 = 18;
/// The automation config sector — above the PID node's two (7 MiB,
/// 7 MiB + 4 KiB) so the two images do not clobber each other's state.
const CONFIG_SECTOR: u32 = 0x0070_2000;
const ANNOUNCE_MS: u64 = 60_000;

struct EspClock;
impl leviculum_core::traits::Clock for EspClock {
    fn now_ms(&self) -> u64 {
        leviculum_esp::uptime_ms()
    }
}

#[main]
fn main() -> ! {
    let peripherals = leviculum_esp::init();
    let mut log = UsbLog::new(peripherals.USB_DEVICE);
    log.identify(&xiao_s3::CONFIG);
    let mut led = Output::new(peripherals.GPIO21, Level::High, OutputConfig::default());

    // ---- 1. pins → HAL ------------------------------------------------
    let mut ow_pin = Flex::new(peripherals.GPIO2);
    ow_pin.apply_output_config(&OutputConfig::default().with_drive_mode(DriveMode::OpenDrain));
    ow_pin.set_output_enable(true);
    ow_pin.set_input_enable(true);
    ow_pin.set_high();
    let mut sensor = Ds18b20Sensor(ds18b20::Ds18b20::new(ow_pin));
    log.line(
        "[AUTO] ",
        format_args!("sensor=ds18b20 present={}", sensor.0.present()),
    );
    let mut actuator = GpioActuator(Output::new(
        peripherals.GPIO4,
        Level::Low,
        OutputConfig::default(),
    ));
    let mut cfg_store = FlashStore {
        flash: esp_storage::FlashStorage::new(peripherals.FLASH),
        offset: CONFIG_SECTOR,
    };
    let clock = UptimeClock;

    // ---- 2. identity + mesh -------------------------------------------
    let _trng_src = esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let identity = match store::load_identity(&mut cfg_store.flash)
        .and_then(|k| Identity::from_private_key_bytes(&k).ok())
    {
        Some(id) => id,
        None => {
            let mut trng = Trng::try_new().expect("TRNG source held");
            let id = Identity::generate(&mut trng);
            if let Ok(k) = id.private_key_bytes() {
                let _ = store::save_identity(&mut cfg_store.flash, &k);
            }
            id
        }
    };
    let our_dest = leviculum_automation::lxmf::delivery_hash(identity.clone()).unwrap_or([0; 16]);
    {
        let mut hex = [0u8; 32];
        to_hex(&our_dest, &mut hex);
        log.line(
            "[AUTO] ",
            format_args!("dest={}", core::str::from_utf8(&hex).unwrap_or("")),
        );
    }

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
            .configure(LORA_FREQ, LORA_SF, LORA_BW, LORA_CR, 22, LORA_PREAMBLE)
            .is_ok()
        && radio.listen().is_ok();
    log.line(
        "[LORA] ",
        format_args!("state={}", if radio_ok { "up" } else { "down" }),
    );

    let mut node = NodeCoreBuilder::new()
        .identity(identity.clone())
        .build_boxed(
            Trng::try_new().expect("TRNG source held"),
            EspClock,
            MemoryStorage::compact(),
        );
    node.set_interface_name(0, "lora_sx1262".into());
    node.set_interface_hw_mtu(0, 508);
    if let Ok(d) = leviculum_lxmf::LxmfNode::delivery_destination_without_inbox(identity.clone()) {
        node.register_destination(d);
    }
    let mut tx_seq = 0u8;
    let mut next_announce = 0u64;
    let mut reassembler = leviculum_core::rnode::SplitReassembler::new();
    let mut rx_tick = 0u32;
    let mut lora_buf = [0u8; 256];

    // ---- 3. the app ---------------------------------------------------
    let mut app = App::boot(Thermostat::new(), &mut cfg_store, clock_now(), &mut log);

    let mut rx_line = [0u8; 1100]; // `AUTO LXMF <hex>` of a full-size message
    let mut rx_len = 0usize;
    let delay = Delay::new();
    loop {
        // Mesh in.
        if radio_ok {
            match radio.poll_rx(&mut lora_buf) {
                Ok(Rx::Frame { len, status }) => {
                    log.line(
                        "[LORA] ",
                        format_args!("rx len={} rssi={} snr={}", len, status.rssi, status.snr),
                    );
                    if let Some(packet) = reassembler.feed(&lora_buf[..len as usize], rx_tick) {
                        let out = node.handle_packet(InterfaceId(0), &packet);
                        for ev in &out.events {
                            if let NodeEvent::PacketReceived {
                                destination, data, ..
                            } = ev
                            {
                                if destination.as_bytes() == &our_dest {
                                    match app.on_message(
                                        data,
                                        &our_dest,
                                        &mut cfg_store,
                                        clock_now(),
                                        &mut log,
                                    ) {
                                        Inbound::Applied { .. } => {}
                                        Inbound::Rejected => {
                                            log.line("[AUTO] ", format_args!("params=rejected"))
                                        }
                                        Inbound::Ignored => {}
                                    }
                                }
                            }
                        }
                        dispatch(out, &mut radio, &mut log, &mut tx_seq);
                    }
                    rx_tick = rx_tick.wrapping_add(1);
                }
                Ok(Rx::BadCrc) => log.line("[LORA] ", format_args!("rx err=crc")),
                Ok(Rx::Idle) => {}
                Err(_) => {
                    let _ = radio.listen();
                }
            }
            dispatch(node.handle_timeout(), &mut radio, &mut log, &mut tx_seq);
            if clock_now() >= next_announce {
                if let Ok(out) = node.announce_destination(
                    &DestinationHash::new(our_dest),
                    Some(b"leviculum-xiao_s3-auto"),
                ) {
                    dispatch(out, &mut radio, &mut log, &mut tx_seq);
                }
                next_announce = clock_now() + ANNOUNCE_MS;
            }
        }

        // The device.
        let tick = app.tick(&mut sensor, &mut actuator, &clock, &mut log);
        if let Some(report) = tick.report {
            log.line(
                "[AUTO] ",
                format_args!(
                    "t={} sp={} on={} lost={}",
                    report.temperature_c, report.setpoint_c, report.on, report.sensor_lost
                ),
            );
            if let Some(on_air) = app.report_message(&identity, &our_dest, &report, clock_now()) {
                if let Some(target) = app.target() {
                    if radio_ok {
                        if let Ok((_, out)) =
                            node.send_single_packet(&DestinationHash::new(target), &on_air)
                        {
                            dispatch(out, &mut radio, &mut log, &mut tx_seq);
                        }
                    }
                }
            }
            led.toggle();
        }

        // Bench path.
        let mut rx = [0u8; 64];
        let n = log.read_bytes(&mut rx);
        for &b in &rx[..n] {
            if b == b'\n' || b == b'\r' {
                if rx_len > 0 {
                    let line = core::str::from_utf8(&rx_line[..rx_len]).unwrap_or("");
                    handle_line(line.trim(), &mut app, &mut cfg_store, &our_dest, &mut log);
                    rx_len = 0;
                }
            } else if rx_len < rx_line.len() {
                rx_line[rx_len] = b;
                rx_len += 1;
            } else {
                rx_len = 0;
            }
        }

        delay.delay_millis(50);
    }
}

fn clock_now() -> u64 {
    leviculum_esp::uptime_ms()
}

/// `AUTO TARGET <32hex>` sets the report destination; `AUTO GET` prints
/// the running params; `AUTO LXMF <hex>` feeds an opportunistic LXMF
/// params message in over the wire the radio would have used — the
/// bench path (`thermostat_host --bench-hex` produces the hex).
fn handle_line<S: ConfigStore>(
    line: &str,
    app: &mut App<Thermostat>,
    store: &mut S,
    our_dest: &[u8; 16],
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
                    "[AUTO] ",
                    format_args!("cmd=target result=ok persisted={ok}"),
                );
            } else {
                log.line("[AUTO] ", format_args!("cmd=target result=bad-hash"));
            }
        }
        Some("LXMF") => {
            let mut data = [0u8; 512];
            let n = unhex(it.next().unwrap_or(""), &mut data);
            let r = match app.on_message(&data[..n], our_dest, store, clock_now(), log) {
                Inbound::Applied { persisted, .. } => {
                    if persisted {
                        "ok"
                    } else {
                        "ok-not-persisted"
                    }
                }
                Inbound::Rejected => "rejected",
                Inbound::Ignored => "not-lxmf",
            };
            log.line("[AUTO] ", format_args!("cmd=lxmf result={r}"));
        }
        Some("GET") => {
            let p = app.device().params();
            log.line(
                "[AUTO] ",
                format_args!(
                    "sp={} hyst={} sample_ms={} report_every={}",
                    p.setpoint_c, p.hysteresis_c, p.sample_ms, p.report_every
                ),
            );
        }
        _ => {}
    }
}

/// `TickOutput` → air. Same as `xiao_s3`'s.
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
                125_000,
                LORA_SF,
                5,
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
