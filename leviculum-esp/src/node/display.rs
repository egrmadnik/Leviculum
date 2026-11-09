//! The LNode's status display: the shared `leviculum-screen` model on
//! the board's I2C OLED, rendered from the main loop's banner tick.
//!
//! The XIAO ESP32-S3 kit has no panel of its own, but its Qwiic add-on
//! pair is the natural home for an I2C OLED: the panel shares the
//! sensor bus, so this module builds the driver over the [`SharedI2c`]
//! handle `peripherals::start` returned, and renders once per banner
//! tick (5 s) — the same cadence as the V4 probe and the nRF boards.
//!
//! Rendering lives in the main loop, not a task, because the bus is
//! shared with the sensors: both are synchronous, never-yielding calls
//! on the same thread, so they can never interleave (the `SharedI2c`
//! contract). The ~20 ms a full blit takes once per 5 s is noise next
//! to the loop's 100 ms pass.
//!
//! Two pages, switched by the wiring's button (polled every loop pass,
//! debounced):
//!
//! - **page 0** — the node's status: identity short hash, LoRa RX/TX
//!   counters, and the painter's absence fields for the things the
//!   XIAO kit does not carry;
//! - **page 1** — the message log: the last few received commands and
//!   the replies sent (the Blink app feeds it through [`log_exchange`]).
//!
//! The page-1 text is drawn with the shared painter's `paint_lines`, so
//! it uses the same FONT_6X10 and line pitch as the status page.

use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use leviculum_screen::{paint_lines, BatteryStatus, GnssStatus, StatusModel};

use crate::display::{Frame, Ssd1315, OLED_WIDTH};
use crate::node::board::DisplayWiring;
use crate::node::carriers;
use crate::SharedI2c;
use crate::UsbLog;

/// One received-command / reply pair, shown on page 1.
struct Exchange {
    received: heapless::String<24>,
    replied: heapless::String<24>,
}

/// The message log: the last [`EXCHANGES`] pairs. All access is from the
/// main loop thread (the Blink app's reply path runs in the responder,
/// the render in the banner tick), so a plain mutex is the whole
/// synchronisation.
const EXCHANGES: usize = 3;

static LOG: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::RefCell<heapless::Vec<Exchange, EXCHANGES>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::RefCell::new(heapless::Vec::new()));

/// Record one received command and its reply for page 1. Called by the
/// Blink app's reply path; the longest 24 characters of each side fit
/// the panel's line width.
pub fn log_exchange(received: &[u8], replied: &[u8]) {
    LOG.lock(|cell| {
        let mut log = cell.borrow_mut();
        if log.len() == EXCHANGES {
            log.remove(0);
        }
        let _ = log.push(Exchange {
            received: truncate(received),
            replied: truncate(replied),
        });
    });
}

/// Up to 24 bytes as a line, truncating mid-character if needed.
fn truncate(bytes: &[u8]) -> heapless::String<24> {
    let mut s = heapless::String::new();
    for &b in bytes.iter().take(24) {
        if s.push(b as char).is_err() {
            break;
        }
    }
    s
}

/// The LNode's OLED, or the whole feature's nothing when no panel is
/// fitted or it refused to come up.
pub struct Display {
    driver: Ssd1315<'static, SharedI2c<'static>>,
    /// The identity's short prefix (`ident_short`), for the id line.
    id_short: heapless::String<10>,
    heartbeat: bool,
    /// The page-switch button, low when pressed.
    button: Option<Input<'static>>,
    /// 0 = status, 1 = message log.
    page: u8,
    /// Consecutive samples the button has been held, for debounce.
    press_samples: u8,
}

impl Display {
    /// Bring the panel up over the shared bus. `None` when the wiring
    /// has no display or the init sequence failed — the caller logs the
    /// reason and the node runs on without a screen.
    pub fn new(
        wiring: DisplayWiring,
        identity_hash: [u8; 16],
        bus: SharedI2c<'static>,
        log: &mut UsbLog<'static>,
    ) -> Option<Self> {
        let reset = wiring
            .reset
            .map(|pin| Output::new(pin, Level::High, OutputConfig::default()));
        let button = wiring
            .button
            .map(|pin| Input::new(pin, InputConfig::default().with_pull(Pull::Up)));
        let mut driver = Ssd1315::new(bus, reset);
        match driver.init() {
            Ok(addr) => {
                log.line("[OLED] ", format_args!("state=up addr=0x{addr:02x}"));
                Some(Self {
                    driver,
                    id_short: leviculum_screen::ident_short(&identity_hash),
                    heartbeat: false,
                    button,
                    page: 0,
                    press_samples: 0,
                })
            }
            Err(_) => {
                log.line("[OLED] ", format_args!("state=down reason=init"));
                None
            }
        }
    }

    /// Poll the page-switch button. Called every main-loop pass (~100
    /// ms): the button must be held for two consecutive samples and then
    /// released before the page flips, which filters contact bounce and
    /// accidental taps.
    pub fn poll_button(&mut self) {
        let Some(button) = self.button.as_mut() else {
            return;
        };
        if button.is_low() {
            self.press_samples = (self.press_samples + 1).min(3);
        } else if self.press_samples >= 2 {
            self.page ^= 1;
            self.press_samples = 0;
        } else {
            self.press_samples = 0;
        }
    }

    /// Paint the current page and blit it. Page 0 is the shared status
    /// model; page 1 the message log. A failed blit is logged, never
    /// fatal.
    pub fn render(&mut self, log: &mut UsbLog<'static>) {
        let mut fb = Frame::new();
        match self.page {
            0 => self.paint_status(&mut fb),
            _ => self.paint_log(&mut fb),
        }
        if self.driver.frame(&fb).is_err() {
            log.line("[OLED] ", format_args!("state=err reason=write"));
        }
    }

    fn paint_status(&mut self, fb: &mut Frame) {
        self.heartbeat = !self.heartbeat;
        let model = StatusModel {
            title: "leviculum XIAO S3",
            id_short: self.id_short.as_str(),
            rx: carriers::RX_PACKETS.load(core::sync::atomic::Ordering::Relaxed),
            tx: carriers::TX_PACKETS.load(core::sync::atomic::Ordering::Relaxed),
            battery: BatteryStatus::FeatureOff,
            gnss: GnssStatus::NoHardware,
            ble_peers: None,
            pn_fill: None,
            heartbeat: self.heartbeat,
        };
        let _ = model.paint(fb, OLED_WIDTH as u32);
    }

    fn paint_log(&self, fb: &mut Frame) {
        let mut lines: [heapless::String<24>; leviculum_screen::LINES] =
            core::array::from_fn(|_| heapless::String::new());
        let mut n = 0;
        LOG.lock(|cell| {
            for exchange in cell.borrow().iter() {
                for line in [&exchange.received, &exchange.replied] {
                    if n < lines.len() {
                        lines[n] = line.clone();
                        n += 1;
                    }
                }
            }
        });
        if n == 0 {
            lines[0] = heapless::String::try_from("no traffic yet").unwrap();
        }
        let _ = paint_lines(fb, &lines);
    }
}
