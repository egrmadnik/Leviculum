//! Blink app: the board's LED flashes at a frequency a remote node sets by
//! LXMF message, every change is acknowledged by reply and saved to flash.
//! Selected by the `lxmf-blink` cargo feature. Same architecture as
//! `leviculum-nrf/src/node/blink.rs`: two Embassy tasks and one reply step
//! in the main loop's LXMF responder.
//!
//! - [`led_task`] owns the LED pin and flashes it; the latest frequency
//!   reaches it through [`TARGET`], a `Signal` (the newest value wins).
//! - [`persist_task`] waits for the frequency to stop changing for
//!   [`SAVE_QUIET`], then writes it to flash — one write for a burst of
//!   commands.
//! - [`reply`] runs in the main loop's responder: it parses the command,
//!   signals both tasks and builds the acknowledgement. The LXMF node
//!   cannot move into a task, so only the LED and the flash write are
//!   tasks.
//!
//! The XIAO ESP32-S3 has one LED, the yellow user LED on GPIO21 (active
//! low), which the status banner otherwise blinks. This app owns it
//! (`OWNS_STATUS_LED`), so the banner stops blinking it.
//!
//! Command, as plain text: `blink <Hz>`. The frequency is 0.1–10 Hz, or `0`
//! for off; fractions are fine (`blink 0.5`). Frequencies are kept in
//! hundredths of a hertz.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU16, Ordering};

use alloc::string::String;
use alloc::vec::Vec;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_hal::gpio::{Level, Output};
use leviculum_core::node::NodeEvent;
use leviculum_core::DestinationHash;

use super::{App, Ctx};
use crate::node::board::AppWiring;
use crate::node::responder::Responder;
use crate::node::storage::{self, SharedFlash};
use crate::store;

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

/// What the reply step and the tasks want logged. They cannot reach the
/// USB log themselves (the main loop owns it), so they leave a line here
/// and [`BlinkApp::tick`] writes it.
enum LogEvent {
    Set(u16),
    Saved(u16),
    SaveFailed(u16),
}

static LOG: Channel<CriticalSectionRawMutex, LogEvent, 8> = Channel::new();

fn valid(centi_hz: u16) -> bool {
    centi_hz == 0 || (MIN_CENTI_HZ..=MAX_CENTI_HZ).contains(&centi_hz)
}

/// Hundredths of a hertz, shown as decimal hertz (`0.50`, `10.00`).
struct Hz(u16);

impl core::fmt::Display for Hz {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

pub struct BlinkApp {
    responder: Responder,
}

impl App for BlinkApp {
    const OWNS_STATUS_LED: bool = true;

    fn start(ctx: &mut Ctx<'_>, w: AppWiring) -> Self {
        let led = w.led.expect("the Blink app owns the status LED");
        // Read the stored frequency (a blank, corrupt or out-of-range record
        // means the default) and spawn the two tasks.
        let (initial, source) = match ctx.with_flash(store::load_blink) {
            Some(c) if valid(c) => (c, "stored"),
            _ => (DEFAULT_CENTI_HZ, "default"),
        };
        CURRENT.store(initial, Ordering::Relaxed);
        ctx.log.line(
            "[BLINK] ",
            format_args!("start hz={} src={}", Hz(initial), source),
        );
        ctx.spawner
            .spawn(led_task(led, w.led_active_high, initial).expect("led task pool"));
        ctx.spawner
            .spawn(persist_task(ctx.flash).expect("persist task pool"));

        BlinkApp {
            responder: Responder::register(ctx, TITLE, reply)
                .expect("[LXMF] inbox registration failed"),
        }
    }

    fn delivery_hash(&self) -> DestinationHash {
        self.responder.delivery_hash()
    }

    fn announce_data(&self) -> &'static [u8] {
        b"leviculum-xiao_s3-blink"
    }

    fn on_events(&mut self, ctx: &mut Ctx<'_>, events: &[NodeEvent]) {
        self.responder.on_events(ctx, events);
    }

    fn tick(&mut self, ctx: &mut Ctx<'_>, _now_ms: u64) {
        while let Ok(event) = LOG.try_receive() {
            match event {
                LogEvent::Set(c) => ctx.log.line("[BLINK] ", format_args!("set hz={}", Hz(c))),
                LogEvent::Saved(c) => ctx.log.line("[BLINK] ", format_args!("saved hz={}", Hz(c))),
                LogEvent::SaveFailed(c) => ctx
                    .log
                    .line("[BLINK] ", format_args!("save FAILED hz={}", Hz(c))),
            }
        }
    }
}

/// Flashes the LED. The LED may be active low: `lit` maps to the level that
/// conducts.
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>, active_high: bool, initial: u16) {
    let level = |lit: bool| {
        if lit == active_high {
            Level::High
        } else {
            Level::Low
        }
    };
    let mut centi = initial;
    let mut lit = false;
    loop {
        if centi == 0 {
            led.set_level(level(false));
            lit = false;
            centi = TARGET.wait().await;
            continue;
        }
        lit = !lit;
        led.set_level(level(lit));
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
async fn persist_task(flash: &'static SharedFlash) {
    loop {
        let mut value = SAVE.wait().await;
        while let Either::First(newer) = select(SAVE.wait(), Timer::after(SAVE_QUIET)).await {
            value = newer;
        }
        let saved = storage::with(flash, |f| store::save_blink(f, value)).is_ok();
        let _ = LOG.try_send(if saved {
            LogEvent::Saved(value)
        } else {
            LogEvent::SaveFailed(value)
        });
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
            TARGET.signal(centi); // the LED task
            SAVE.signal(centi); // the persist task
            if centi == 0 {
                let _ = write!(out, "blink set: off");
            } else {
                let _ = write!(
                    out,
                    "blink set: {} Hz (period {} ms)",
                    Hz(centi),
                    100_000 / centi as u32
                );
            }
            let _ = LOG.try_send(LogEvent::Set(centi));
        }
        Err(why) => {
            let _ = write!(
                out,
                "blink error: {}; still {} Hz",
                why,
                Hz(CURRENT.load(Ordering::Relaxed))
            );
        }
    }
    out.into_bytes()
}
