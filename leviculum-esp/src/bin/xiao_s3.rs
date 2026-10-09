//! Firmware binary for the Seeed XIAO ESP32-S3 + Wio-SX1262 kit.
//!
//! Same step-1 skeleton as the V4's binary, plus one more claim: bring
//! the SoC up, open the USB Serial/JTAG port, say which commit and which
//! board this image is, take the radio pins the kit's B2B connector
//! assigns and hold the SX1262's SPI port open, probe the I2C pair once
//! for the optional BMV080 particulate sensor — then keep saying it every
//! five seconds so a capture attached after the boot window still reads
//! the board.
//!
//! The radio is **live**: [`leviculum_esp::radio::Radio`] brings the
//! SX1262 up on the `eu_medium` Reticulum profile (869.463 MHz, SF8,
//! BW125, 22 dBm), parks it in continuous RX, and a boxed
//! `NodeCore` runs the same transport engine the daemon and the nRF
//! boards run — inbound packets arrive via `handle_packet`, outbound
//! `TickOutput` actions become `build_lora_frames` + `transmit`. The
//! PID loop's two wire formats ride it directly: an LXMF message whose
//! `content` is a `PIDC` frame retunes the loop (and remembers the
//! sender as the report target), and every sample emits a signed LXMF
//! status message with a `PIDS` frame back to it.
//!
//! What is still absent, honestly: no CSMA (`SetCad` exists in core,
//! not wired here), no announce-cap queueing, no path persistence —
//! `MemoryStorage::compact` means a reboot relearns the mesh. The
//! receive path is polled on DIO1 rather than interrupt-driven.
//!
//! The pin handles are moved out of `Peripherals` through the board
//! module's type aliases rather than by their GPIO numbers. That is not
//! decoration: `let nss: xiao_s3::LoRaNss = peripherals.GPIO41` is a
//! compile-time assertion that this binary and the board file agree about
//! which pin carries `NSS`, and it is the only place the two can be held
//! to each other.

#![no_std]
#![no_main]

use esp_hal::gpio::{DriveMode, Flex};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
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
use leviculum_core::{
    node::{NodeCoreBuilder, NodeEvent},
    transport::{Action, TickOutput},
    DestinationHash, Identity, InterfaceId, MemoryStorage,
};
use leviculum_esp::{
    bme690, bmv080,
    boards::xiao_s3,
    ds18b20, pid, pid_lxmf,
    radio::{Radio, Rx},
    store,
    sx1262::Sx1262Bus,
    UsbLog,
};

// The application descriptor the second-stage bootloader reads. Without it
// `espflash save-image` produces an image the ROM declines.
esp_bootloader_esp_idf::esp_app_desc!();

/// `leviculum_core::traits::Clock` over `uptime_ms` — monotonic only;
/// `wall_unix_secs` stays `None`, so announce emission timestamps come
/// from the transport's learned timebase, which is the intended answer
/// on a board with no RTC.
/// The LoRa profile — the `eu_medium` numbers the fleet agrees on
/// (ReticulumNet consensus channel, SF8/BW125/CR4/5, the 18-symbol
/// preamble `derive_preamble_symbols` lands on).
const LORA_FREQ: u32 = 869_463_000;
const LORA_SF: u8 = 8;
const LORA_BW: u8 = 0x04; // 125 kHz
const LORA_CR: u8 = 0x01; // 4/5
const LORA_PREAMBLE: u16 = 18;

struct EspClock;

impl leviculum_core::traits::Clock for EspClock {
    fn now_ms(&self) -> u64 {
        leviculum_esp::uptime_ms()
    }
}

/// How often the board re-states what it is.
///
/// Five seconds, the same cadence the nRF boards use, for the same reason:
/// a reader that attaches after the boot window — which is every reader,
/// since the port only enumerates once the firmware is running — must not
/// have to reset the board to learn what is on it.
const BANNER_INTERVAL_MS: u32 = 5_000;

#[main]
fn main() -> ! {
    let peripherals = leviculum_esp::init();

    // First thing after init, and over the port the flasher reads back.
    let mut log = UsbLog::new(peripherals.USB_DEVICE);
    log.identify(&xiao_s3::CONFIG);

    // The status LED: the only boot proof available on a board running
    // from a battery with no cable attached. On this kit it is the
    // module's yellow USER_LED on GPIO21, **active low** — parked high.
    let mut led: Output = Output::new(
        peripherals.GPIO21,
        if xiao_s3::LED_ACTIVE_HIGH {
            Level::Low
        } else {
            Level::High
        },
        OutputConfig::default(),
    );

    // ---- the SX1262's port, opened and left alone ----------------------
    let sck: xiao_s3::LoRaSck = peripherals.GPIO7;
    let mosi: xiao_s3::LoRaMosi = peripherals.GPIO9;
    let miso: xiao_s3::LoRaMiso = peripherals.GPIO8;
    let nss_pin: xiao_s3::LoRaNss = peripherals.GPIO41;
    let busy_pin: xiao_s3::LoRaBusy = peripherals.GPIO40;
    let reset_pin: xiao_s3::LoRaReset = peripherals.GPIO42;
    let dio1_pin: xiao_s3::LoRaDio1 = peripherals.GPIO39;
    let rxen_pin: xiao_s3::LoRaRxEnable = peripherals.GPIO38;

    // Chip-select high before the bus exists: a low line while the SoC is
    // still booting is the start of a command the chip will wait to
    // finish.
    let nss = Output::new(nss_pin, Level::High, OutputConfig::default());
    let reset = Output::new(reset_pin, Level::High, OutputConfig::default());
    // BUSY and DIO1 are outputs of the radio; no pull, the chip drives
    // them.
    let busy = Input::new(busy_pin, InputConfig::default().with_pull(Pull::None));
    let dio1 = Input::new(dio1_pin, InputConfig::default().with_pull(Pull::None));
    // The RF switch's host side: high steers the antenna to the RX path,
    // low during TX where DIO2 takes the switch. Parked low until the
    // radio's `listen` asserts it.
    let rxen = Output::new(rxen_pin, Level::Low, OutputConfig::default());

    let spi = match Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(xiao_s3::LORA_SPI_FREQ_HZ))
            // SX126x: CPOL=0, CPHA=0 (datasheet rev 2.1 §13.1).
            .with_mode(Mode::_0),
    ) {
        Ok(spi) => spi
            .with_sck(sck)
            .with_mosi(mosi)
            .with_miso(miso)
            .into_async(),
        Err(_) => {
            // The clock divider could not be met. Say so and keep the
            // banner running: a board that boots and reports a broken bus
            // is diagnosable, one that halts is not.
            log.line("[LORA] ", format_args!("state=down reason=spi-config"));
            loop {
                blink(&mut led);
                Delay::new().delay_millis(BANNER_INTERVAL_MS);
                log.identify(&xiao_s3::CONFIG);
            }
        }
    };

    // The radio, live this step: init → configure → continuous RX, the
    // same order the nRF boards run on the same `leviculum_core::sx126x`
    // sequences. Failure leaves the board logging `[LORA] state=down`
    // and running everything else — a sensor node without a mesh is
    // degraded, not dead.
    let mut radio = Radio::new(Sx1262Bus::new(spi, nss, busy), reset, dio1, rxen);
    let radio_ok = match radio.init(LORA_FREQ) {
        Ok(chip_status) => {
            match radio.configure(LORA_FREQ, LORA_SF, LORA_BW, LORA_CR, 22, LORA_PREAMBLE) {
                Ok(program) => {
                    let _ = radio.listen();
                    log.line(
                        "[LORA] ",
                        format_args!(
                            "state=up chip=0x{:02X} freq={} sf={} bw=125k power={}dBm",
                            chip_status, LORA_FREQ, LORA_SF, program.programmed_dbm
                        ),
                    );
                    true
                }
                Err(_) => {
                    log.line("[LORA] ", format_args!("state=down reason=configure"));
                    false
                }
            }
        }
        Err(_) => {
            log.line("[LORA] ", format_args!("state=down reason=init"));
            false
        }
    };

    // ---- the BMV080's port, opened and asked once ---------------------
    // A BMV080 is an add-on on this kit (the Qwiic/I2C pads, D4/D5), so
    // presence is a runtime question and the answer is printed, not
    // assumed — the same `no-hardware` discipline the nRF boards apply
    // to GNSS. Everything past the ACK — handshake, measurement,
    // duty-cycling — is Bosch's `lib_bmv080.a`, a licensed blob this
    // crate does not carry; `bmv080::probe` draws that boundary.
    let sda_pin: xiao_s3::I2cSda = peripherals.GPIO5;
    let scl_pin: xiao_s3::I2cScl = peripherals.GPIO6;
    let mut i2c = match I2c::new(peripherals.I2C0, I2cConfig::default()) {
        Ok(i2c) => i2c.with_sda(sda_pin).with_scl(scl_pin),
        Err(_) => {
            log.line("[BMV080] ", format_args!("state=down reason=i2c-config"));
            loop {
                blink(&mut led);
                Delay::new().delay_millis(BANNER_INTERVAL_MS);
                log.identify(&xiao_s3::CONFIG);
            }
        }
    };

    // Both probes run first — before the bus is handed out — because
    // `SharedI2c::new` is the moment `i2c` stops being borrowable by name.
    let bmv080_present = bmv080::probe(&mut i2c) == bmv080::Probe::Present;
    let bme690_addr = bme690::probe(&mut i2c);

    // The bus is shared: BMV080 and BME690 both live on these two pins,
    // so it is handed out as a `SharedI2c` — one handle per driver,
    // exclusivity enforced by sequencing rather than by the type system
    // (see `SharedI2c` in lib.rs).
    #[cfg(any(feature = "bmv080-sdk", feature = "bme690"))]
    let bus = leviculum_esp::SharedI2c::new(&mut i2c);

    // Present is the only outcome that can go further, and only when the
    // build carries the SDK (`--features bmv080-sdk`); otherwise the
    // probe line is the whole BMV080 story for this image.
    #[cfg(feature = "bmv080-sdk")]
    let mut sensor = match bmv080_present {
        true => match bmv080::sdk::Sensor::open(bus) {
            Ok(mut sensor) => {
                let (maj, min, pat) = bmv080::sdk::Sensor::driver_version();
                let id = sensor.sensor_id().unwrap_or([b'?'; 13]);
                log.line(
                    "[BMV080] ",
                    format_args!(
                        "state=up drv={}.{}.{} id={}",
                        maj,
                        min,
                        pat,
                        core::str::from_utf8(&id).unwrap_or("?")
                    ),
                );
                // Duty cycling: 10 s integration window every 60 s —
                // the SDK's own defaults, sized for a battery node.
                if let Err(st) = sensor
                    .configure_duty_cycle(10.0, 60)
                    .and_then(|_| sensor.start_duty_cycling())
                {
                    log.line(
                        "[BMV080] ",
                        format_args!("state=down reason=start status={st}"),
                    );
                    None
                } else {
                    Some(sensor)
                }
            }
            Err(st) => {
                log.line(
                    "[BMV080] ",
                    format_args!("state=down reason=open status={st}"),
                );
                None
            }
        },
        false => {
            log.line(
                "[BMV080] ",
                format_args!(
                    "state=absent addr=0x{:02x} note=not-fitted-or-moved-jumper",
                    bmv080::I2C_ADDRESS
                ),
            );
            None
        }
    };
    #[cfg(not(feature = "bmv080-sdk"))]
    if bmv080_present {
        log.line(
            "[BMV080] ",
            format_args!(
                "state=present addr=0x{:02x} note=probe-only-no-sdk",
                bmv080::I2C_ADDRESS
            ),
        );
    } else {
        log.line(
            "[BMV080] ",
            format_args!(
                "state=absent addr=0x{:02x} note=not-fitted-or-moved-jumper",
                bmv080::I2C_ADDRESS
            ),
        );
    }

    // ---- the BME690, on the same bus ---------------------------------
    // Temperature/pressure/humidity/gas — an open-source driver this
    // time, so `--features bme690` compiles it rather than linking a
    // blob. Forced mode, one sample per banner tick.
    #[cfg(feature = "bme690")]
    let mut bme = match bme690_addr {
        Some(addr) => match bme690::api::Sensor::open(bus, addr) {
            Ok(mut sensor) => {
                let chip = sensor.chip_id();
                log.line(
                    "[BME690] ",
                    format_args!("state=up addr=0x{addr:02x} chip=0x{chip:02x}"),
                );
                Some(sensor)
            }
            Err(st) => {
                log.line(
                    "[BME690] ",
                    format_args!("state=down reason=init status={st}"),
                );
                None
            }
        },
        None => {
            log.line(
                "[BME690] ",
                format_args!("state=absent addr=0x76|0x77 note=not-fitted"),
            );
            None
        }
    };
    #[cfg(not(feature = "bme690"))]
    match bme690_addr {
        Some(addr) => {
            log.line(
                "[BME690] ",
                format_args!("state=present addr=0x{addr:02x} note=probe-only-no-sdk"),
            );
        }
        None => {
            log.line(
                "[BME690] ",
                format_args!("state=absent addr=0x76|0x77 note=not-fitted"),
            );
        }
    }

    // ---- the PID loop -----------------------------------------------
    // DS18B20 on D1 (open-drain, external 4.7 kΩ pull-up), valve on D3.
    // Config comes from flash when it was ever set — a remote node's
    // tune survives reset — else the default; the serial lines it
    // answers are documented at `parse_pid_command`.
    let mut ow_pin = Flex::new(peripherals.GPIO2);
    ow_pin.apply_output_config(&OutputConfig::default().with_drive_mode(DriveMode::OpenDrain));
    ow_pin.set_input_enable(true);
    ow_pin.set_output_enable(false);
    let mut temp_sensor = ds18b20::Ds18b20::new(ow_pin);
    log.line(
        "[DS18B20] ",
        if temp_sensor.present() {
            format_args!("state=present pin=D1")
        } else {
            format_args!("state=absent pin=D1 note=no-presence-pulse")
        },
    );

    let mut valve = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);

    // ---- the node's LXMF identity -----------------------------------
    // Loaded from its own flash sector, or generated once by the TRNG —
    // the destination hash a remote node addresses is only stable if
    // the identity is.
    // The TRNG source has to be held for `Trng::try_new` to succeed —
    // it unlocks only while a TrngSource exists.
    let _trng_src = esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let identity = match store::load_identity(&mut flash)
        .and_then(|k| Identity::from_private_key_bytes(&k).ok())
    {
        Some(id) => {
            log.line("[PID] ", format_args!("identity=restored"));
            id
        }
        None => {
            let mut trng = Trng::try_new().expect("TRNG source enabled above");
            let id = Identity::generate(&mut trng);
            let persisted = id
                .private_key_bytes()
                .ok()
                .and_then(|k| store::save_identity(&mut flash, &k).ok())
                .is_some();
            log.line(
                "[PID] ",
                format_args!("identity=generated persisted={persisted}"),
            );
            id
        }
    };
    let our_dest = match pid_lxmf::delivery_hash(identity.clone()) {
        Some(h) => {
            let mut hex = [0u8; 32];
            to_hex(&h, &mut hex);
            log.line(
                "[PID] ",
                format_args!("dest={}", core::str::from_utf8(&hex).unwrap_or("")),
            );
            h
        }
        None => {
            log.line("[PID] ", format_args!("dest=unavailable"));
            [0u8; 16]
        }
    };

    let mut lora_buf = [0u8; 256];
    let mut reassembler = leviculum_core::rnode::SplitReassembler::new();
    let mut rx_tick = 0u32;
    let mut tx_seq = 0u8;
    let mut next_announce = 30_000u64;

    // ---- the Reticulum node ------------------------------------------
    // One interface, the LoRa radio (InterfaceId(0)). The engine is the
    // same `NodeCore` the host daemon and nRF boards run — packet
    // dispatch is the `dispatch` fn at the bottom of this file, which
    // maps `Action::SendPacket`/`Broadcast` onto `radio.transmit` and
    // feeds inbound frames back through `handle_packet`.
    let mut node = NodeCoreBuilder::new()
        .identity(identity.clone())
        .build_boxed(
            Trng::try_new().expect("TRNG source enabled above"),
            EspClock,
            MemoryStorage::compact(),
        );
    if radio_ok {
        node.set_interface_name(0, "lora_sx1262".into());
        // 508 — the RNode HW_MTU the whole fleet agrees on (#390).
        node.set_interface_hw_mtu(0, 508);
        if let Ok(dest) =
            leviculum_lxmf::LxmfNode::delivery_destination_without_inbox(identity.clone())
        {
            node.register_destination(dest);
        }
        let out = node.announce_destination(
            &DestinationHash::new(our_dest),
            Some(b"leviculum-xiao_s3-pid"),
        );
        if let Ok(out) = out {
            dispatch(out, &mut radio, &mut log, &mut tx_seq);
        }
        log.line("[LORA] ", format_args!("mesh=announced"));
    }

    let (cfg, mut report_target) = match store::load_config(&mut flash) {
        Some((c, t)) => {
            log.line(
                "[PID] ",
                format_args!(
                    "state=loaded sp={} kp={} ki={} kd={} sample_ms={} window_ms={}",
                    c.setpoint_c, c.kp, c.ki, c.kd, c.sample_ms, c.window_ms
                ),
            );
            (c, t)
        }
        None => {
            log.line(
                "[PID] ",
                format_args!(
                    "state=default sp=22 kp=4 ki=0.05 kd=0 sample_ms=2000 window_ms=10000"
                ),
            );
            (pid::Config::DEFAULT, None)
        }
    };
    let mut pid = pid::Pid::new(cfg);
    let mut duty = 0.0f32;
    let mut last_t: Option<f32> = None;
    let mut last_sample_ms = 0u64;
    let mut status = pid::Status {
        temperature_c: 0.0,
        setpoint_c: cfg.setpoint_c,
        duty: 0.0,
        output_on: false,
        sensor_lost: true,
        uptime_s: 0,
    };
    let mut next_sample = 0u64;
    let mut rx_line = [0u8; 128];
    let mut rx_len = 0usize;

    #[cfg(feature = "bmv080-sdk")]
    let mut sink = bmv080::sdk::Measurement::default();

    let delay = Delay::new();
    let mut next_banner = 0u64;
    loop {
        #[cfg(feature = "bmv080-sdk")]
        {
            // The SDK wants serve_interrupt at least once a second; it
            // drains the event FIFO and is cheap when nothing is due.
            if let Some(sensor) = sensor.as_mut() {
                let seq = sink.seq;
                match sensor.serve(&mut sink) {
                    Ok(()) if sink.seq != seq => {
                        log.line(
                            "[BMV080] ",
                            format_args!(
                                "pm1={} pm2_5={} pm10={} obstructed={} outofrange={}",
                                sink.pm1_ug_m3,
                                sink.pm2_5_ug_m3,
                                sink.pm10_ug_m3,
                                sink.obstructed as u8,
                                sink.outside_range as u8,
                            ),
                        );
                    }
                    Ok(()) => {}
                    Err(st) => {
                        log.line(
                            "[BMV080] ",
                            format_args!("state=err reason=serve status={st}"),
                        );
                    }
                }
            }
        }

        // ---- mesh ingress ---------------------------------------------
        // Poll the radio, hand complete Reticulum packets to NodeCore,
        // dispatch whatever comes back (rebroadcasts, our own traffic).
        // The `PacketReceived` events are the LXMF deliveries: a `PIDC`
        // frame in `content` retunes the loop.
        if radio_ok {
            match radio.poll_rx(&mut lora_buf) {
                Ok(Rx::Frame { len, status }) => {
                    log.line(
                        "[LORA] ",
                        format_args!("rx len={} rssi={} snr={}", len, status.rssi, status.snr),
                    );
                    if let Some(packet) = reassembler.feed(&lora_buf[..len as usize], rx_tick) {
                        let out = node.handle_packet(InterfaceId(0), &packet);
                        handle_node_events(
                            &out,
                            &mut log,
                            &mut pid,
                            &mut flash,
                            &mut report_target,
                            &our_dest,
                        );
                        dispatch(out, &mut radio, &mut log, &mut tx_seq);
                    }
                    rx_tick = rx_tick.wrapping_add(1);
                }
                Ok(Rx::BadCrc) => {
                    log.line("[LORA] ", format_args!("rx err=crc"));
                }
                Ok(Rx::Idle) => {}
                Err(_) => {
                    log.line("[LORA] ", format_args!("rx err=bus"));
                    let _ = radio.listen();
                }
            }
            let out = node.handle_timeout();
            handle_node_events(
                &out,
                &mut log,
                &mut pid,
                &mut flash,
                &mut report_target,
                &our_dest,
            );
            dispatch(out, &mut radio, &mut log, &mut tx_seq);

            // Re-announce so the fleet keeps our path warm.
            if leviculum_esp::uptime_ms() >= next_announce {
                let out = node.announce_destination(
                    &DestinationHash::new(our_dest),
                    Some(b"leviculum-xiao_s3-pid"),
                );
                if let Ok(out) = out {
                    dispatch(out, &mut radio, &mut log, &mut tx_seq);
                }
                next_announce = leviculum_esp::uptime_ms() + 60_000;
            }
        }

        // ---- command ingress ------------------------------------------
        // The debug port still takes `PID SET kp=.. ki=.. kd=.. sp=..
        // sample_ms=.. window_ms=..`, `PID GET`, `PID REPORT`, `PID
        // LXMF <hex>`, `PID TARGET <hex>` — the mesh path feeds the same
        // frames.
        let mut rx = [0u8; 64];
        let n = log.read_bytes(&mut rx);
        for &b in &rx[..n] {
            if b == b'\n' || b == b'\r' {
                if rx_len > 0 {
                    let line = core::str::from_utf8(&rx_line[..rx_len]).unwrap_or("");
                    handle_pid_line(
                        line.trim(),
                        &mut log,
                        &mut pid,
                        &mut flash,
                        &status,
                        &mut report_target,
                        &our_dest,
                    );
                    rx_len = 0;
                }
            } else if rx_len < rx_line.len() {
                rx_line[rx_len] = b;
                rx_len += 1;
            } else {
                // Overrun — the line is garbage either way; drop it.
                rx_len = 0;
            }
        }

        // ---- the PID tick ----------------------------------------------
        let now = leviculum_esp::uptime_ms();
        if now >= next_sample {
            let dt = if last_sample_ms == 0 {
                cfg.sample_ms as f32 / 1000.0
            } else {
                (now - last_sample_ms) as f32 / 1000.0
            };
            last_sample_ms = now;
            next_sample = now + pid.config().sample_ms as u64;

            match temp_sensor.measure() {
                Some(t) => {
                    last_t = Some(t);
                    duty = pid.update(t, dt);
                }
                None => {
                    // Sensor lost: fail the valve closed, flag it. A
                    // stuck open valve on a blind controller is the
                    // failure this refuses.
                    duty = 0.0;
                }
            }
            status = pid::Status {
                temperature_c: last_t.unwrap_or(0.0),
                setpoint_c: pid.config().setpoint_c,
                duty,
                output_on: pid.valve_on(duty, now),
                sensor_lost: last_t.is_none(),
                uptime_s: (now / 1000) as u32,
            };
            valve.set_level(if status.output_on {
                Level::High
            } else {
                Level::Low
            });
            log.line(
                "[PID] ",
                format_args!(
                    "t={} sp={} duty={:.3} out={} {}",
                    status.temperature_c,
                    status.setpoint_c,
                    status.duty,
                    if status.output_on { "on" } else { "off" },
                    if status.sensor_lost {
                        "sensor=lost"
                    } else {
                        ""
                    },
                ),
            );

            // The LXMF status report — `content = PIDS`, on-air
            // opportunistic bytes handed to NodeCore which encrypts and
            // radiates them. The debug port still sees the bytes when
            // the radio is down.
            if let Some(target) = report_target {
                if let Some(on_air) = pid_lxmf::build_status_message(
                    &identity,
                    &our_dest,
                    &target,
                    &status,
                    now as f64 / 1000.0,
                ) {
                    if radio_ok {
                        match node.send_single_packet(&DestinationHash::new(target), &on_air) {
                            Ok((_pkt_hash, out)) => {
                                dispatch(out, &mut radio, &mut log, &mut tx_seq);
                            }
                            Err(_) => {
                                log.line("[PID] ", format_args!("lxmf=send-failed"));
                            }
                        }
                    } else {
                        let mut mhex = [0u8; 1080];
                        let hl = to_hex(&on_air, &mut mhex);
                        log.line(
                            "[PID] ",
                            format_args!(
                                "lxmf={}",
                                core::str::from_utf8(&mhex[..hl]).unwrap_or("")
                            ),
                        );
                    }
                }
            }
        } else {
            // Between samples the valve still needs its window slicing.
            valve.set_level(if pid.valve_on(duty, now) {
                Level::High
            } else {
                Level::Low
            });
        }

        // With either sensor feature on, tick fast — the BMV080 wants
        // its interrupt served at least once a second and the BME690 is
        // a banner-tick reader either way. The PID tick above paces
        // itself on `sample_ms`; 100 ms keeps the valve window honest.
        delay.delay_millis(100);

        if leviculum_esp::uptime_ms() >= next_banner {
            // One forced-mode sample per banner tick — a 5 s cadence the
            // BME690's 100 ms heater soaks in easily.
            #[cfg(feature = "bme690")]
            if let Some(sensor) = bme.as_mut() {
                match sensor.measure() {
                    Ok(Some(m)) => {
                        log.line(
                            "[BME690] ",
                            format_args!(
                                "t={} p={} h={} gas={}",
                                m.temperature_c, m.pressure_pa, m.humidity_pct, m.gas_ohm,
                            ),
                        );
                    }
                    Ok(None) => {}
                    Err(st) => {
                        log.line(
                            "[BME690] ",
                            format_args!("state=err reason=measure status={st}"),
                        );
                    }
                }
            }

            blink(&mut led);
            log.identify(&xiao_s3::CONFIG);
            next_banner = leviculum_esp::uptime_ms() + BANNER_INTERVAL_MS as u64;
        }
    }
}

/// One short flash of the status LED, in whichever direction the board
/// wires it.
fn blink(led: &mut Output<'_>) {
    let delay = Delay::new();
    if xiao_s3::LED_ACTIVE_HIGH {
        led.set_high();
        delay.delay_millis(30);
        led.set_low();
    } else {
        led.set_low();
        delay.delay_millis(30);
        led.set_high();
    }
}

/// One `PID ...` line from the debug port.
///
/// The command set is the text form of the mesh path: a remote node's
/// `PIDC` frame decodes into the same `Config` `PID SET` builds, and
/// `PID IMPORT` takes that frame hex-encoded so the binary wire shape is
/// exercised end-to-end on the bench, not only after transport lands.
///
///   PID SET kp=4 ki=0.05 kd=0 sp=22 sample_ms=2000 window_ms=10000
///       — any subset of keys; unset keys keep their values. A good set
///         applies, then persists to flash.
///   PID GET     — print the live config.
///   PID IMPORT XXYY… — the `PIDC` frame as hex; decode or reject.
///   PID REPORT  — print the `PIDS` status frame as hex, the bytes a
///                 mesh report would carry.
fn handle_pid_line(
    line: &str,
    log: &mut UsbLog<'_>,
    pid: &mut pid::Pid,
    flash: &mut esp_storage::FlashStorage<'_>,
    status: &pid::Status,
    report_target: &mut Option<[u8; 16]>,
    our_dest: &[u8; 16],
) {
    let mut it = line.split_whitespace();
    if it.next() != Some("PID") {
        return;
    }
    match it.next() {
        Some("SET") => {
            let mut cfg = *pid.config();
            let mut bad = false;
            for kv in it {
                let Some((k, v)) = kv.split_once('=') else {
                    bad = true;
                    break;
                };
                let f = |s: &str| s.parse::<f32>();
                let u = |s: &str| s.parse::<u32>();
                let ok = match k {
                    "kp" => f(v).map(|x| cfg.kp = x).is_ok(),
                    "ki" => f(v).map(|x| cfg.ki = x).is_ok(),
                    "kd" => f(v).map(|x| cfg.kd = x).is_ok(),
                    "sp" => f(v).map(|x| cfg.setpoint_c = x).is_ok(),
                    "sample_ms" => u(v).map(|x| cfg.sample_ms = x).is_ok(),
                    "window_ms" => u(v).map(|x| cfg.window_ms = x).is_ok(),
                    _ => false,
                };
                if !ok {
                    bad = true;
                    break;
                }
            }
            if bad || cfg.sample_ms == 0 || cfg.window_ms == 0 {
                log.line("[PID] ", format_args!("cmd=set result=reject"));
                return;
            }
            pid.set_config(cfg, leviculum_esp::uptime_ms());
            match store::save_config(flash, &cfg, report_target.as_ref()) {
                Ok(()) => log.line(
                    "[PID] ",
                    format_args!(
                        "cmd=set result=ok persisted=yes sp={} kp={} ki={} kd={}",
                        cfg.setpoint_c, cfg.kp, cfg.ki, cfg.kd
                    ),
                ),
                Err(_) => log.line("[PID] ", format_args!("cmd=set result=ok persisted=no")),
            }
        }
        Some("GET") => {
            let c = pid.config();
            log.line(
                "[PID] ",
                format_args!(
                    "sp={} kp={} ki={} kd={} sample_ms={} window_ms={}",
                    c.setpoint_c, c.kp, c.ki, c.kd, c.sample_ms, c.window_ms
                ),
            );
        }
        Some("IMPORT") => {
            let hex = it.next().unwrap_or("");
            let mut frame = [0u8; pid::CONFIG_FRAME_LEN];
            if unhex(hex, &mut frame) != pid::CONFIG_FRAME_LEN {
                log.line("[PID] ", format_args!("cmd=import result=bad-hex"));
                return;
            }
            match pid::Config::decode(&frame) {
                Ok(cfg) if cfg.sample_ms > 0 && cfg.window_ms > 0 => {
                    pid.set_config(cfg, leviculum_esp::uptime_ms());
                    let saved = store::save_config(flash, &cfg, report_target.as_ref()).is_ok();
                    log.line(
                        "[PID] ",
                        format_args!("cmd=import result=ok persisted={}", saved),
                    );
                }
                _ => log.line("[PID] ", format_args!("cmd=import result=reject")),
            }
        }
        Some("REPORT") => {
            let frame = status.encode();
            let mut hex = [0u8; pid::STATUS_FRAME_LEN * 2];
            let _ = to_hex(&frame, &mut hex);
            log.line(
                "[PID] ",
                format_args!("frame={}", core::str::from_utf8(&hex).unwrap_or("")),
            );
        }
        Some("LXMF") => {
            // `PID LXMF <hex>` — a packed LXMF message as the remote
            // node would radiate it. Same decode the transport will use.
            let hex = it.next().unwrap_or("");
            let mut data = [0u8; 512];
            let n = unhex(hex, &mut data);
            match pid_lxmf::decode_config_message(&data[..n], our_dest) {
                pid_lxmf::Inbound::Config(cfg, src) => {
                    pid.set_config(cfg, leviculum_esp::uptime_ms());
                    *report_target = Some(src);
                    let saved = store::save_config(flash, &cfg, Some(&src)).is_ok();
                    let mut shex = [0u8; 32];
                    to_hex(&src, &mut shex);
                    log.line(
                        "[PID] ",
                        format_args!(
                            "cmd=lxmf result=ok target={} persisted={}",
                            core::str::from_utf8(&shex).unwrap_or(""),
                            saved
                        ),
                    );
                }
                pid_lxmf::Inbound::NotOurs => {
                    log.line("[PID] ", format_args!("cmd=lxmf result=not-ours"));
                }
                pid_lxmf::Inbound::BadFrame => {
                    log.line("[PID] ", format_args!("cmd=lxmf result=bad-frame"));
                }
            }
        }
        Some("TARGET") => {
            // `PID TARGET <32-hex>` — set the report destination manually.
            let mut t = [0u8; 16];
            if unhex(it.next().unwrap_or(""), &mut t) == 16 {
                *report_target = Some(t);
                let cfg = *pid.config();
                let saved = store::save_config(flash, &cfg, Some(&t)).is_ok();
                log.line(
                    "[PID] ",
                    format_args!("cmd=target result=ok persisted={saved}"),
                );
            } else {
                log.line("[PID] ", format_args!("cmd=target result=bad-hash"));
            }
        }
        _ => {}
    }
}

/// hex → bytes; returns bytes written.
fn unhex(s: &str, out: &mut [u8]) -> usize {
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut n = 0;
    while n * 2 + 1 < bytes.len() && n < out.len() {
        match (nib(bytes[n * 2]), nib(bytes[n * 2 + 1])) {
            (Some(h), Some(l)) => {
                out[n] = h << 4 | l;
                n += 1;
            }
            _ => break,
        }
    }
    n
}

fn to_hex(bytes: &[u8], out: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut n = 0;
    for &b in bytes {
        if n + 2 > out.len() {
            break;
        }
        out[n] = HEX[(b >> 4) as usize];
        out[n + 1] = HEX[(b & 0xf) as usize];
        n += 2;
    }
    n
}

/// What a `TickOutput` becomes on this board: every `SendPacket` and
/// `Broadcast` goes onto the air as RNode-framed LoRa (the 254-byte
/// split a Reticulum packet is entitled to), and every `PacketReceived`
/// event has already been dealt with by [`handle_node_events`].
///
/// After a transmit the chip is in standby — `listen` re-arms the
/// continuous RX the loop expects.
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

/// The application half of a `TickOutput`: events, and of them only
/// `PacketReceived` matters — a decrypted delivery to a registered
/// destination. Ours is `lxmf.delivery`; its payload is an LXMF
/// message whose `content` is a `PIDC` frame, handled by the same
/// `decode_config_message` the debug port's `PID LXMF` feeds.
fn handle_node_events(
    out: &TickOutput,
    log: &mut UsbLog<'_>,
    pid: &mut pid::Pid,
    flash: &mut esp_storage::FlashStorage<'_>,
    report_target: &mut Option<[u8; 16]>,
    our_dest: &[u8; 16],
) {
    for event in &out.events {
        let NodeEvent::PacketReceived {
            destination, data, ..
        } = event
        else {
            continue;
        };
        if destination.as_bytes() != our_dest {
            continue;
        }
        match pid_lxmf::decode_config_message(data, our_dest) {
            pid_lxmf::Inbound::Config(cfg, src) => {
                pid.set_config(cfg, leviculum_esp::uptime_ms());
                *report_target = Some(src);
                let saved = store::save_config(flash, &cfg, Some(&src)).is_ok();
                let mut shex = [0u8; 32];
                to_hex(&src, &mut shex);
                log.line(
                    "[PID] ",
                    format_args!(
                        "cmd=mesh result=ok from={} persisted={}",
                        core::str::from_utf8(&shex).unwrap_or(""),
                        saved
                    ),
                );
            }
            pid_lxmf::Inbound::NotOurs => {
                log.line("[PID] ", format_args!("cmd=mesh result=not-ours"));
            }
            pid_lxmf::Inbound::BadFrame => {
                log.line("[PID] ", format_args!("cmd=mesh result=bad-frame"));
            }
        }
    }
}
