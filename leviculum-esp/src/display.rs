//! The OLED status displays: a hand-rolled SSD1315 driver over I2C,
//! painting the shared `leviculum-screen` status model.
//!
//! Two boards, two bus arrangements, one driver:
//!
//! - the **Heltec V4** carries a 0.96" 128×64 OLED (nets
//!   `OLED_SDA`/`OLED_SCL`/`OLED_RST`, `boards/heltec_v4.rs`) on an
//!   I2C bus of its own;
//! - the **XIAO ESP32-S3 kit** has no panel, but its Qwiic add-on pair
//!   (D4/D5) is the natural home for an I2C OLED — that bus is shared
//!   with the sensors, so the driver writes through a
//!   [`SharedI2c`](crate::SharedI2c) handle instead of owning the
//!   peripheral.
//!
//! The controller is an SSD1306-family part on both — same command set,
//! same page-addressed RAM — so the driver is the standard 128×64
//! sequence and a page-wise blit. What the panel shows is not this
//! crate's drawing: it is [`leviculum_screen::StatusModel`], the same
//! six-line model + heartbeat dot the nRF boards paint, locked by the
//! same golden-frame host tests. This module only owns the wire.
//!
//! The `display` feature gates the whole module.

use esp_hal::gpio::Output;
use esp_hal::i2c::master::I2c;
use esp_hal::Blocking;
use leviculum_screen::MonoFb;

use crate::SharedI2c;

/// The OLED's I2C address with SA0 strapped low — the SSD1306-family
/// default. [`Ssd1315::init`] probes this and [`OLED_ADDRESS_SA0_HIGH`]
/// and keeps whichever answers, so a panel strapped either way works.
pub const OLED_ADDRESS_SA0_LOW: u8 = 0x3C;
/// The OLED's I2C address with SA0 strapped high.
pub const OLED_ADDRESS_SA0_HIGH: u8 = 0x3D;
/// Panel width in pixels.
pub const OLED_WIDTH: usize = 128;
/// Panel height in pixels.
pub const OLED_HEIGHT: usize = 64;
/// RAM pages: 8 rows of 8 vertical pixels.
const OLED_PAGES: u8 = 8;

/// The standard 128×64 SSD1306-family init sequence the SSD1315 (V4)
/// and SSD1306 (XIAO add-on) panels both accept. Sent once at the
/// address that answers; a NACK on the wrong address does nothing.
const INIT: &[u8] = &[
    0xAE, // display off
    0xD5, 0x80, // clock divide ratio
    0xA8, 0x3F, // multiplex ratio 1/64
    0xD3, 0x00, // display offset 0
    0x40, // start line 0
    0x8D, 0x14, // charge pump on
    0x20, 0x00, // horizontal addressing mode
    0xA1, // segment remap (column 127 → SEG0)
    0xC8, // COM scan direction remapped
    0xDA, 0x12, // COM pins: alternative, no left/right remap
    0x81, 0xCF, // contrast
    0xD9, 0xF1, // pre-charge period
    0xDB, 0x40, // VCOMH deselect level
    0xA4, // resume to RAM content
    0xA6, // normal (not inverted) display
    0xAF, // display on
];

/// The monochrome framebuffer the painter draws into and this driver
/// blits. Row-major MSB-first, matching `MonoFb`'s own layout.
pub type Frame = MonoFb<OLED_WIDTH, OLED_HEIGHT, { OLED_WIDTH / 8 * OLED_HEIGHT }>;

/// Anything the panel can be written through: an owned blocking I2C bus
/// (the V4) or a shared handle to the sensor bus (the XIAO kit's Qwiic
/// add-on). Both expose the same one-call write.
pub trait BusWrite {
    /// One blocking master write.
    fn write(&mut self, address: u8, buffer: &[u8]) -> Result<(), esp_hal::i2c::master::Error>;
}

impl BusWrite for I2c<'_, Blocking> {
    fn write(&mut self, address: u8, buffer: &[u8]) -> Result<(), esp_hal::i2c::master::Error> {
        self.write(address, buffer)
    }
}

impl BusWrite for SharedI2c<'_> {
    fn write(&mut self, address: u8, buffer: &[u8]) -> Result<(), esp_hal::i2c::master::Error> {
        SharedI2c::write(self, address, buffer)
    }
}

/// The SSD1315/SSD1306 on a blocking I2C bus, with its reset line.
pub struct Ssd1315<'d, W: BusWrite> {
    bus: W,
    reset: Option<Output<'d>>,
    /// The address the panel answered at during [`init`].
    address: u8,
}

impl<'d, W: BusWrite> Ssd1315<'d, W> {
    /// Take the bus and the reset line. Nothing is sent yet — [`init`]
    /// owns the command sequence, so a caller can log a failed init and
    /// keep running without a panel. A panel without a reset net (the
    /// XIAO's Qwiic connector has none) passes `None`.
    pub fn new(bus: W, reset: Option<Output<'d>>) -> Self {
        Self {
            bus,
            reset,
            address: OLED_ADDRESS_SA0_LOW,
        }
    }

    /// The address the panel answered at, once [`init`] succeeded.
    pub fn address(&self) -> u8 {
        self.address
    }

    /// Drive reset (low pulse, then release — skipped when the wiring
    /// carries no reset net), then run the init sequence at whichever
    /// of the two SSD1306-family addresses answers: a panel strapped
    /// SA0-low sits at [`OLED_ADDRESS_SA0_LOW`], SA0-high at
    /// [`OLED_ADDRESS_SA0_HIGH`], and a NACK on the wrong address is
    /// harmless. Returns the resolved address.
    pub fn init(&mut self) -> Result<u8, esp_hal::i2c::master::Error> {
        if let Some(reset) = self.reset.as_mut() {
            reset.set_low();
            esp_hal::delay::Delay::new().delay_millis(10);
            reset.set_high();
            esp_hal::delay::Delay::new().delay_millis(10);
        }
        self.address = OLED_ADDRESS_SA0_LOW;
        if self.cmd(INIT).is_ok() {
            return Ok(self.address);
        }
        self.address = OLED_ADDRESS_SA0_HIGH;
        self.cmd(INIT)?;
        Ok(self.address)
    }

    /// Blit the whole frame. The panel's RAM is page-addressed (each
    /// byte is a vertical column of 8 pixels, LSB = top row), while
    /// [`Frame`] is row-major — the transpose is done here, one page at
    /// a time: page address, column 0, then 128 data bytes.
    pub fn frame(&mut self, fb: &Frame) -> Result<(), esp_hal::i2c::master::Error> {
        for page in 0..OLED_PAGES {
            self.cmd(&[0xB0 | page, 0x00, 0x10])?;
            let mut data = [0u8; OLED_WIDTH + 1];
            data[0] = 0x40; // data control byte
            for x in 0..OLED_WIDTH {
                let mut b = 0u8;
                for bit in 0..8 {
                    if fb.pixel(x, page as usize * 8 + bit) {
                        b |= 1 << bit;
                    }
                }
                data[x + 1] = b;
            }
            self.bus.write(self.address, &data)?;
        }
        Ok(())
    }

    /// One command batch: the control byte (0x00 = command) then the
    /// commands, in a single transaction.
    fn cmd(&mut self, cmds: &[u8]) -> Result<(), esp_hal::i2c::master::Error> {
        let mut buf = [0u8; 1 + 32];
        buf[0] = 0x00;
        buf[1..1 + cmds.len()].copy_from_slice(cmds);
        self.bus.write(self.address, &buf[..1 + cmds.len()])
    }
}

/// Linear single-cell percent from the divider-recovered pack voltage:
/// 3.0 V → 0 %, 4.2 V → 100 %.
///
/// The V4 is a 1S board (the U4 charger charges one cell), so this is
/// the whole classification. A voltage the classification cannot
/// explain (below 3.0 V — a reading that would otherwise render as a
/// negative percent) yields `None`, so the panel shows the voltage
/// with `--%` beside it rather than a number nobody can check: the
/// same asymmetry the nRF painter's `BatteryStatus::Data` documents
/// (`percent` is optional, `voltage_mv` is not).
pub fn battery_percent(voltage_mv: u16) -> Option<u8> {
    if !(3000..=4200).contains(&voltage_mv) {
        return None;
    }
    Some((((voltage_mv - 3000) as u32 * 100) / 1200) as u8)
}
