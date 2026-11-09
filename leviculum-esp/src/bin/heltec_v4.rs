//! Firmware binary for the Heltec WiFi LoRa 32 V4.
//!
//! What it does, and it is the whole of what this step claims: bring the
//! SoC up, open the USB Serial/JTAG port, say which commit and which board
//! this image is, take the seven radio pins the schematic assigns and hold
//! the SX1262's SPI port open, then keep saying it every five seconds so a
//! capture attached after the boot window still reads the board.
//!
//! **Nothing is transmitted and nothing is asked of the radio.** The bus is
//! constructed, not used: chip-select is parked high, reset is parked
//! inactive, and no opcode is issued. What the construction proves is that
//! [`leviculum_esp::sx1262::Sx1262Bus`] satisfies
//! [`leviculum_core::sx126x`]'s two traits against esp-hal's real SPI type
//! and the real pins — which is the property that makes step 2 wiring
//! instead of design.
//!
//! With the `display` feature, the board's SSD1315 OLED shows the same
//! `leviculum-screen` status model the nRF boards paint (title, build
//! sha, rx/tx, battery, heartbeat), refreshed at the banner cadence. The
//! panel is driven even when the radio bus refused to configure — a dead
//! radio and a live screen is a diagnosable board.
//!
//! The pin handles are moved out of `Peripherals` through the board
//! module's type aliases rather than by their GPIO numbers. That is not
//! decoration: `let nss: heltec_v4::LoRaNss = peripherals.GPIO8` is a
//! compile-time assertion that this binary and the board file agree about
//! which pin carries `LoRa_NSS`, and it is the only place the two can be
//! held to each other.

#![no_std]
#![no_main]

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
use leviculum_esp::{boards::heltec_v4, sx1262::Sx1262Bus, UsbLog};

// The application descriptor the second-stage bootloader reads. Without it
// `espflash save-image` produces an image the ROM declines.
esp_bootloader_esp_idf::esp_app_desc!();

/// How often the board re-states what it is.
///
/// Five seconds, the same cadence the nRF boards use, for the same reason:
/// a reader that attaches after the boot window — which is every reader,
/// since the port only enumerates once the firmware is running — must not
/// have to reset the board to learn what is on it.
const BANNER_INTERVAL_MS: u32 = 5_000;

/// The short git sha this image was built from, for the OLED's id line —
/// the probe's stand-in for an identity (it has none).
const GIT_SHA: &str = env!("LEVICULUM_GIT_SHA");

#[cfg(feature = "display")]
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
#[cfg(feature = "display")]
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
#[cfg(feature = "display")]
use leviculum_esp::display::{battery_percent, Frame, Ssd1315, OLED_WIDTH};
#[cfg(feature = "display")]
use leviculum_screen::{BatteryStatus, GnssStatus, StatusModel};

#[main]
fn main() -> ! {
    let peripherals = leviculum_esp::init();

    // First thing after init, and over the port the flasher reads back.
    let mut log = UsbLog::new(peripherals.USB_DEVICE);
    log.identify(&heltec_v4::CONFIG);

    // The status LED: the only boot proof available on a board running
    // from a battery with no cable attached.
    let mut led: Output = Output::new(
        peripherals.GPIO35,
        if heltec_v4::LED_ACTIVE_HIGH {
            Level::Low
        } else {
            Level::High
        },
        OutputConfig::default(),
    );

    // ---- the OLED status display ---------------------------------------
    // Feature-gated: only the V4 has a panel. A display that refuses to
    // come up is logged and the board runs on without it — the OLED is
    // diagnostics, never the reason the firmware stops.
    #[cfg(feature = "display")]
    let mut screen: Option<
        Ssd1315<'static, esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>>,
    > = {
        let sda: heltec_v4::OledSda = peripherals.GPIO17;
        let scl: heltec_v4::OledScl = peripherals.GPIO18;
        let reset: heltec_v4::OledReset = peripherals.GPIO21;
        match I2c::new(
            peripherals.I2C0,
            I2cConfig::default().with_frequency(Rate::from_hz(400_000)),
        ) {
            Ok(i2c) => {
                let mut d = Ssd1315::new(
                    i2c.with_sda(sda).with_scl(scl),
                    Some(Output::new(reset, Level::High, OutputConfig::default())),
                );
                match d.init() {
                    Ok(addr) => {
                        log.line("[OLED] ", format_args!("state=up addr=0x{addr:02x}"));
                        Some(d)
                    }
                    Err(_) => {
                        log.line("[OLED] ", format_args!("state=down reason=init"));
                        None
                    }
                }
            }
            Err(_) => {
                log.line("[OLED] ", format_args!("state=down reason=i2c-config"));
                None
            }
        }
    };
    #[cfg(feature = "display")]
    let mut adc_cfg = AdcConfig::new();
    #[cfg(feature = "display")]
    let mut battery_pin = adc_cfg.enable_pin(peripherals.GPIO1, Attenuation::_11dB);
    #[cfg(feature = "display")]
    let mut adc = Adc::new(peripherals.ADC1, adc_cfg);
    #[cfg(feature = "display")]
    let mut battery_enable = Output::new(peripherals.GPIO37, Level::Low, OutputConfig::default());
    #[cfg(feature = "display")]
    let mut heartbeat = false;

    // ---- the SX1262's port, opened and left alone ----------------------
    let sck: heltec_v4::LoRaSck = peripherals.GPIO9;
    let mosi: heltec_v4::LoRaMosi = peripherals.GPIO10;
    let miso: heltec_v4::LoRaMiso = peripherals.GPIO11;
    let nss_pin: heltec_v4::LoRaNss = peripherals.GPIO8;
    let busy_pin: heltec_v4::LoRaBusy = peripherals.GPIO13;
    let reset_pin: heltec_v4::LoRaReset = peripherals.GPIO12;
    let dio1_pin: heltec_v4::LoRaDio1 = peripherals.GPIO14;

    // Chip-select high before the bus exists: a low line while the SoC is
    // still booting is the start of a command the chip will wait to
    // finish.
    let nss = Output::new(nss_pin, Level::High, OutputConfig::default());
    // Reset parked inactive (the line is active low). Driven rather than
    // left floating — an undriven reset on a chip nobody has spoken to yet
    // is a board whose state depends on leakage.
    let _reset = Output::new(reset_pin, Level::High, OutputConfig::default());
    // BUSY and DIO1 are outputs of the radio; no pull, the chip drives
    // them.
    let busy = Input::new(busy_pin, InputConfig::default().with_pull(Pull::None));
    let _dio1 = Input::new(dio1_pin, InputConfig::default().with_pull(Pull::None));

    let spi = match Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(heltec_v4::LORA_SPI_FREQ_HZ))
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
            let delay = Delay::new();
            loop {
                #[cfg(feature = "display")]
                render_screen(
                    screen.as_mut(),
                    &mut adc,
                    &mut battery_pin,
                    &mut battery_enable,
                    &mut heartbeat,
                    &delay,
                    &mut log,
                );
                blink(&mut led);
                delay.delay_millis(BANNER_INTERVAL_MS);
                log.identify(&heltec_v4::CONFIG);
            }
        }
    };

    // Constructed and held. Nothing is issued on it at this step; the
    // binding exists so the trait impls are monomorphised against the real
    // SPI type and the real pins.
    let _radio = Sx1262Bus::new(spi, nss, busy);
    log.line(
        "[LORA] ",
        format_args!(
            "state=idle bus=spi2 hz={} note=no-traffic-this-step",
            heltec_v4::LORA_SPI_FREQ_HZ
        ),
    );

    let delay = Delay::new();
    loop {
        #[cfg(feature = "display")]
        render_screen(
            screen.as_mut(),
            &mut adc,
            &mut battery_pin,
            &mut battery_enable,
            &mut heartbeat,
            &delay,
            &mut log,
        );
        blink(&mut led);
        delay.delay_millis(BANNER_INTERVAL_MS);
        log.identify(&heltec_v4::CONFIG);
    }
}

/// Paint one status frame and blit it to the panel, at the banner
/// cadence. Everything the probe knows that the shared painter's model
/// can carry: the build sha as the id line, the battery from a one-shot
/// SAADC read through the switched divider, and no GNSS/BLE/propagation
/// (the V4 probe carries none of them). A failed read or blit is
/// logged, never fatal.
#[cfg(feature = "display")]
fn render_screen<'d>(
    screen: Option<&mut Ssd1315<'static, esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>>>,
    adc: &mut Adc<'d, esp_hal::peripherals::ADC1<'d>, esp_hal::Blocking>,
    battery_pin: &mut esp_hal::analog::adc::AdcPin<
        heltec_v4::BatterySensePin<'d>,
        esp_hal::peripherals::ADC1<'d>,
    >,
    battery_enable: &mut Output<'static>,
    heartbeat: &mut bool,
    delay: &Delay,
    log: &mut UsbLog<'static>,
) {
    let Some(display) = screen else { return };
    // One-shot battery: switch the divider on (active high), let it
    // settle, read, switch it off — the divider only draws current while
    // a reading is being taken.
    battery_enable.set_high();
    delay.delay_millis(1);
    let battery = match adc.read_oneshot(battery_pin).ok() {
        Some(raw) => {
            battery_enable.set_low();
            // Raw 12-bit counts → millivolts at the pin (approximate:
            // 11 dB attenuation, full scale ~3100 mV, no efuse curve),
            // then through the 390K/100K divider (×4.9).
            let pin_mv = raw as u32 * 3100 / 4095;
            let pack_mv = pin_mv * 49 / 10;
            BatteryStatus::Data {
                percent: battery_percent(pack_mv as u16),
                voltage_mv: pack_mv as u16,
            }
        }
        None => {
            battery_enable.set_low();
            BatteryStatus::NoData
        }
    };
    *heartbeat = !*heartbeat;
    let model = StatusModel {
        title: "leviculum V4",
        id_short: GIT_SHA,
        rx: 0,
        tx: 0,
        battery,
        gnss: GnssStatus::NoHardware,
        ble_peers: None,
        pn_fill: None,
        heartbeat: *heartbeat,
    };
    let mut fb = Frame::new();
    if model.paint(&mut fb, OLED_WIDTH as u32).is_err() {
        return;
    }
    if display.frame(&fb).is_err() {
        log.line("[OLED] ", format_args!("state=err reason=write"));
    }
}

/// One short flash of the status LED, in whichever direction the board
/// wires it.
fn blink(led: &mut Output<'_>) {
    let delay = Delay::new();
    if heltec_v4::LED_ACTIVE_HIGH {
        led.set_high();
        delay.delay_millis(30);
        led.set_low();
    } else {
        led.set_low();
        delay.delay_millis(30);
        led.set_high();
    }
}
