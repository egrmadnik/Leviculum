//! Firmware binary for the Seeed XIAO ESP32-S3 + Wio-SX1262 kit.
//!
//! Same step-1 skeleton as the V4's binary: bring the SoC up, open the
//! USB Serial/JTAG port, say which commit and which board this image is,
//! take the radio pins the kit's B2B connector assigns and hold the
//! SX1262's SPI port open, then keep saying it every five seconds so a
//! capture attached after the boot window still reads the board.
//!
//! **Nothing is transmitted and nothing is asked of the radio.** The bus
//! is constructed, not used: chip-select is parked high, reset is parked
//! inactive, the RF switch is parked in its not-receiving position, and
//! no opcode is issued. What the construction proves is that
//! [`leviculum_esp::sx1262::Sx1262Bus`] satisfies
//! [`leviculum_core::sx126x`]'s two traits against esp-hal's real SPI type
//! and the real pins — and on this board it additionally proves the four
//! JTAG pads (GPIO39–42) and the FSPIWP pad (GPIO38) are ours to drive,
//! which is the only thing about this pin map a compile cannot check.
//!
//! The pin handles are moved out of `Peripherals` through the board
//! module's type aliases rather than by their GPIO numbers. That is not
//! decoration: `let nss: xiao_s3::LoRaNss = peripherals.GPIO41` is a
//! compile-time assertion that this binary and the board file agree about
//! which pin carries `NSS`, and it is the only place the two can be held
//! to each other.

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
use leviculum_esp::{boards::xiao_s3, sx1262::Sx1262Bus, UsbLog};

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
    // Reset parked inactive (the line is active low). Driven rather than
    // left floating — an undriven reset on a chip nobody has spoken to yet
    // is a board whose state depends on leakage.
    let _reset = Output::new(reset_pin, Level::High, OutputConfig::default());
    // BUSY and DIO1 are outputs of the radio; no pull, the chip drives
    // them.
    let busy = Input::new(busy_pin, InputConfig::default().with_pull(Pull::None));
    let _dio1 = Input::new(dio1_pin, InputConfig::default().with_pull(Pull::None));
    // The RF switch's host side, parked in its not-receiving position:
    // low means the antenna does not reach the RX path, which is the
    // defined state for a radio nobody is listening on. Asserted only by
    // whoever opens a listening window — nobody does, at this step.
    let _rxen = Output::new(rxen_pin, Level::Low, OutputConfig::default());

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

    // Constructed and held. Nothing is issued on it at this step; the
    // binding exists so the trait impls are monomorphised against the real
    // SPI type and the real pins.
    let _radio = Sx1262Bus::new(spi, nss, busy);
    log.line(
        "[LORA] ",
        format_args!(
            "state=idle bus=spi2 hz={} note=no-traffic-this-step",
            xiao_s3::LORA_SPI_FREQ_HZ
        ),
    );

    let delay = Delay::new();
    loop {
        blink(&mut led);
        delay.delay_millis(BANNER_INTERVAL_MS);
        log.identify(&xiao_s3::CONFIG);
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
