//! The seam between the shared LNode code and one ESP32 board.
//!
//! A board binary implements [`Board`] and calls [`super::run`]. The trait
//! carries what changes from one board to the next (the pin map, the radio
//! profile, the LED polarity) and nothing else; everything the LNode does
//! on every board lives in the sibling modules.
//!
//! Pins travel as type-erased [`AnyPin`]s. The board's own `wire` is where
//! `let nss: xiao_s3::LoRaNss = p.GPIO41;` pins each net to a named alias,
//! which keeps the compile-time check that the binary and the board file
//! agree on which GPIO carries which net.

use esp_hal::gpio::{AnyPin, Output};
use esp_hal::peripherals::{
    Peripherals, ADC1, FLASH, FROM_CPU_INTR0, I2C0, RNG, SPI2, TIMG0, USB_DEVICE, WIFI,
};

use crate::boards::BoardConfig;

/// The LoRa channel parameters a board runs on.
#[derive(Clone, Copy)]
pub struct LoraProfile {
    pub freq_hz: u32,
    pub sf: u8,
    /// SX126x bandwidth register code (`0x04` is 125 kHz).
    pub bw_code: u8,
    pub bw_hz: u32,
    /// SX126x coding-rate register code (`0x01` is 4/5).
    pub cr_code: u8,
    /// The coding-rate denominator, 5 for 4/5.
    pub cr_denominator: u8,
    pub preamble_symbols: u16,
    pub power_dbm: i8,
}

/// The SX1262's SPI port and control lines.
pub struct LoraWiring {
    pub spi: SPI2<'static>,
    pub sck: AnyPin<'static>,
    pub mosi: AnyPin<'static>,
    pub miso: AnyPin<'static>,
    /// Chip select, active low.
    pub nss: AnyPin<'static>,
    pub busy: AnyPin<'static>,
    pub reset: AnyPin<'static>,
    pub dio1: AnyPin<'static>,
    /// The RF switch's host side: high steers the antenna to the RX path.
    pub rxen: AnyPin<'static>,
    pub spi_freq_hz: u32,
}

/// The I2C pair the add-on sensors share.
pub struct SensorWiring {
    pub i2c: I2C0<'static>,
    pub sda: AnyPin<'static>,
    pub scl: AnyPin<'static>,
}

/// An I2C OLED fitted on the board (`display` feature).
///
/// The panel shares the board's I2C bus — on the XIAO kit that is the
/// Qwiic add-on pair, on the V4 the OLED's own nets — so this bundle
/// deliberately carries no I2C peripheral and no SDA/SCL pins: the bus
/// is opened once by `peripherals::start` and the display writes
/// through the shared handle. What is this board's to declare is the
/// panel's reset net (the Qwiic connector has none) and the page-switch
/// button, when the board carries one (the XIAO kit's user button).
pub struct DisplayWiring {
    /// OLED reset, active low; `None` when the panel exposes no reset
    /// net (a Qwiic-connected panel).
    pub reset: Option<AnyPin<'static>>,
    /// The button that flips the display between its pages, low when
    /// pressed (the display polls it with a pull-up); `None` on boards
    /// without one.
    pub button: Option<AnyPin<'static>>,
}

/// Pins only the apps use: the PID app's 1-Wire temperature input and
/// valve, and the status LED when the app owns it (the Blink app).
pub struct AppWiring {
    pub onewire: AnyPin<'static>,
    pub valve: AnyPin<'static>,
    /// The status LED, filled in by [`super::run`] for an app that owns it
    /// (`App::OWNS_STATUS_LED`); a board leaves this `None`.
    pub led: Option<Output<'static>>,
    /// The LED's polarity, filled in by [`super::run`].
    pub led_active_high: bool,
}

/// The SoC peripherals the shared code takes for itself.
pub struct System {
    pub usb: USB_DEVICE<'static>,
    pub flash: FLASH<'static>,
    /// The TRNG needs both of these for as long as the node lives.
    pub rng: RNG<'static>,
    pub adc1: ADC1<'static>,
    /// The scheduler's tick source and software interrupt
    /// (`esp_rtos::start`).
    pub timg0: TIMG0<'static>,
    pub sw_int0: FROM_CPU_INTR0<'static>,
    /// The WiFi radio (`wifi` feature: `node::wifi`).
    pub wifi: WIFI<'static>,
}

/// What [`Board::wire`] returns: every peripheral the shared code takes.
pub struct Wiring {
    pub system: System,
    pub led: AnyPin<'static>,
    pub lora: LoraWiring,
    pub sensors: SensorWiring,
    /// The fitted OLED, if any (`display` feature; the XIAO kit's
    /// Qwiic add-on, the V4's own panel).
    pub display: Option<DisplayWiring>,
    pub app: AppWiring,
}

/// One hardware unit.
pub trait Board {
    /// Name used in the boot banner.
    const NAME: &'static str;
    /// Runtime metadata for the `[BOARD]` line.
    const CONFIG: &'static BoardConfig;
    /// The LoRa channel.
    const LORA: LoraProfile;
    /// Whether the status LED conducts when driven high.
    const LED_ACTIVE_HIGH: bool;

    /// Split `p` into the shared peripherals and this board's own pins.
    /// Pure pin bookkeeping: nothing here touches hardware.
    fn wire(p: Peripherals) -> Wiring;
}
