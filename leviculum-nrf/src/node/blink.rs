//! Blink app: the board's blink LED flashes at a frequency a remote node
//! sets by LXMF message, every change is acknowledged by reply and saved to
//! flash. Selected by the `lxmf-blink` cargo feature.
//!
//! Two Embassy tasks and one app step in the main loop:
//!
//! - [`led_task`] owns the LED pin and flashes it; the latest frequency
//!   reaches it through [`TARGET`], a `Signal` (the newest value wins).
//! - [`persist_task`] waits for the frequency to stop changing for
//!   [`SAVE_QUIET`], then asks the telemetry store task to write it to
//!   flash — one write for a burst of commands.
//! - [`reply`] runs in the main loop's LXMF responder: it parses the
//!   command, signals both tasks and builds the acknowledgement. The LXMF
//!   node cannot move into a task, so only the LED and the flash write are
//!   tasks.
//!
//! Command, as plain text: `blink <Hz>`. The frequency is 0.1–10 Hz, or
//! `0` for off; fractions are fine (`blink 0.5`). Frequencies are kept in
//! hundredths of a hertz.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU16, Ordering};

use alloc::string::String;
use alloc::vec::Vec;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_nrf::gpio::{Level, Output};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};

use crate::log_critical;

/// LXMF title of a blink reply.
pub const TITLE: &[u8] = b"blink";

/// Frequency at boot when nothing valid is stored: 1 Hz.
pub const DEFAULT_CENTI_HZ: u16 = 100;
/// Slowest non-zero frequency: 0.1 Hz.
const MIN_CENTI_HZ: u16 = 10;
/// Fastest frequency: 10 Hz.
const MAX_CENTI_HZ: u16 = 1000;
/// How long the frequency must stay unchanged before it is written.
const SAVE_QUIET: Duration = Duration::from_secs(5);

/// New frequency for the LED task, in hundredths of a hertz.
static TARGET: Signal<CriticalSectionRawMutex, u16> = Signal::new();
/// New frequency for the persist task.
static SAVE: Signal<CriticalSectionRawMutex, u16> = Signal::new();
/// The frequency in force, for the replies.
static CURRENT: AtomicU16 = AtomicU16::new(DEFAULT_CENTI_HZ);

fn valid(centi_hz: u16) -> bool {
    centi_hz == 0 || (MIN_CENTI_HZ..=MAX_CENTI_HZ).contains(&centi_hz)
}

/// Read the stored frequency (a blank, corrupt or out-of-range record
/// means the default) and spawn the two tasks.
pub fn start(spawner: &Spawner, led: Output<'static>, telemetry_page: u32) {
    let (initial, source) = match crate::telemetry::load_blink(telemetry_page) {
        Some(c) if valid(c) => (c, "stored"),
        _ => (DEFAULT_CENTI_HZ, "default"),
    };
    CURRENT.store(initial, Ordering::Relaxed);
    log_critical!("[BLINK] start hz={} src={}", Hz(initial), source);
    spawner.must_spawn(led_task(led, initial));
    spawner.must_spawn(persist_task());
}

/// Flashes the LED. The LED is active low: `Low` is lit.
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>, initial: u16) {
    let mut centi = initial;
    let mut lit = false;
    loop {
        if centi == 0 {
            led.set_level(Level::High);
            lit = false;
            centi = TARGET.wait().await;
            continue;
        }
        lit = !lit;
        led.set_level(if lit { Level::Low } else { Level::High });
        // Half a period: 1000 ms / hz / 2 = 50 000 / centi-hertz.
        let half = Duration::from_millis(50_000 / centi as u64);
        if let Either::First(new) = select(TARGET.wait(), Timer::after(half)).await {
            // Restart the phase on a change so it takes effect at once.
            centi = new;
            lit = false;
        }
    }
}

/// Writes the frequency to flash once it has stopped changing.
#[embassy_executor::task]
async fn persist_task() {
    loop {
        let mut value = SAVE.wait().await;
        while let Either::First(newer) = select(SAVE.wait(), Timer::after(SAVE_QUIET)).await {
            value = newer;
        }
        let save = crate::telemetry::request_save_blink(value);
        match crate::telemetry::confirm(save).await {
            leviculum_core::envelope::Persist::Durable => {
                log_critical!("[BLINK] saved hz={}", Hz(value));
            }
            _ => log_critical!("[BLINK] save FAILED hz={}", Hz(value)),
        }
    }
}

/// Hundredths of a hertz, shown as decimal hertz (`0.50`, `10.00`).
struct Hz(u16);

impl core::fmt::Display for Hz {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

/// Parse `blink <Hz>` into hundredths of a hertz.
fn parse(content: &[u8]) -> Result<u16, &'static str> {
    let text = core::str::from_utf8(content).map_err(|_| "not text")?;
    let mut words = text.split_whitespace();
    match words.next() {
        Some(w) if w.eq_ignore_ascii_case("blink") => {}
        _ => return Err("expected: blink <Hz>"),
    }
    let value = words.next().ok_or("expected: blink <Hz>")?;
    if words.next().is_some() {
        return Err("expected: blink <Hz>");
    }
    let hz: f32 = value.parse().map_err(|_| "not a number")?;
    // Written so that NaN fails: every comparison with it is false.
    if !(0.0..=10.0).contains(&hz) {
        return Err("out of range 0.1-10 Hz, or 0 for off");
    }
    let centi = (hz * 100.0 + 0.5) as u16;
    if valid(centi) {
        Ok(centi)
    } else {
        Err("out of range 0.1-10 Hz, or 0 for off")
    }
}

/// The responder's reply function: apply the command, acknowledge it.
pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut out = String::new();
    match parse(content) {
        Ok(centi) => {
            CURRENT.store(centi, Ordering::Relaxed);
            TARGET.signal(centi);
            SAVE.signal(centi);
            if centi == 0 {
                let _ = write!(out, "blink set: off");
            } else {
                let _ = write!(out, "blink set: {} Hz (period {} ms)", Hz(centi), 100_000 / centi as u32);
            }
            log_critical!("[BLINK] set hz={}", Hz(centi));
        }
        Err(why) => {
            let _ = write!(out, "blink error: {}; still {} Hz", why, Hz(CURRENT.load(Ordering::Relaxed)));
        }
    }
    out.into_bytes()
}
