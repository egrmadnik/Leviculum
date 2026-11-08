//! The app seam: what an application on an ESP LNode implements.
//!
//! The main loop is an async function on the Embassy executor: it polls
//! the radio, hands the app each batch of node events, gives it a line
//! typed on the debug port and calls its `tick`, then sleeps. An app that
//! needs real concurrency (a blinking LED, a flash writer) spawns Embassy
//! tasks from `start` with `ctx.spawner`; the loop's methods must not
//! block.
//!
//! The applications themselves live beside this file:
//!
//! | module | app |
//! |---|---|
//! | `pid_app` | the PID valve regulator (default) |
//! | `blink` | the LED blinker (`lxmf-blink` feature) |
//! | `echo` | the LXMF echo inbox (`lxmf-echo` feature) |

#[cfg(feature = "lxmf-blink")]
pub mod blink;
#[cfg(feature = "lxmf-echo")]
pub mod echo;
#[cfg(not(any(feature = "lxmf-echo", feature = "lxmf-blink")))]
pub mod pid_app;

use embassy_executor::Spawner;
use leviculum_core::node::NodeEvent;
use leviculum_core::transport::TickOutput;
use leviculum_core::{DestinationHash, Identity};

use crate::node::board::AppWiring;
use crate::node::carriers::Carrier;
use crate::node::reticulum::Node;
use crate::node::storage::{self, SharedFlash};
use crate::UsbLog;

/// Everything an app may touch, borrowed for one call.
pub struct Ctx<'a> {
    pub node: &'a mut Node,
    pub carrier: &'a mut Carrier,
    pub log: &'a mut UsbLog<'static>,
    pub flash: &'static SharedFlash,
    pub identity: &'a Identity,
    /// For spawning the app's own tasks.
    pub spawner: Spawner,
}

impl Ctx<'_> {
    /// Put the actions of a `TickOutput` on the air: the WiFi socket
    /// tasks take theirs first (`node::wifi::egress`), the LoRa carrier
    /// what remains.
    pub fn dispatch(&mut self, out: TickOutput) {
        #[cfg(feature = "wifi")]
        let out = crate::node::wifi::egress(out);
        self.carrier.dispatch(out, self.log);
    }

    /// Run `f` with exclusive access to the internal flash.
    pub fn with_flash<R>(&self, f: impl FnOnce(&mut esp_storage::FlashStorage<'static>) -> R) -> R {
        storage::with(self.flash, f)
    }
}

/// An application: it owns its settings and its hardware, and answers the
/// loop's questions.
pub trait App: Sized {
    /// Whether the app drives the status LED itself. If so the loop stops
    /// blinking it at the banner and hands it over in `AppWiring::led`.
    const OWNS_STATUS_LED: bool = false;

    /// How long the loop sleeps between passes, in milliseconds.
    const TICK_MS: u64 = 100;

    /// Take the app's pins, load its settings, register its destination and
    /// spawn its tasks.
    fn start(ctx: &mut Ctx<'_>, wiring: AppWiring) -> Self;

    /// The destination this app announces.
    fn delivery_hash(&self) -> DestinationHash;

    /// The announce's app data (the node's display name).
    fn announce_data(&self) -> &'static [u8];

    /// A batch of node events (deliveries, links). Called before the
    /// batch's actions are dispatched.
    fn on_events(&mut self, _ctx: &mut Ctx<'_>, _events: &[NodeEvent]) {}

    /// One line typed on the debug port.
    fn on_line(&mut self, _ctx: &mut Ctx<'_>, _line: &str) {}

    /// Once per loop pass.
    fn tick(&mut self, _ctx: &mut Ctx<'_>, _now_ms: u64) {}
}
