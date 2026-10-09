//! A portable automation device for an LNode.
//!
//! The shape: a *device* (a regulator, a counter, a scheduler — anything
//! with a config, an input, an output and a status) that lives on a LoRa
//! LNode, takes its parameters from a remote node over LXMF, keeps them
//! in flash across reboots, and reports its status back the same way.
//! Everything about the MCU is behind a trait, so the same device runs
//! on the nRF52840 kit and the ESP32-S3 kit with a wrapper of a few
//! dozen lines each.
//!
//! ```text
//!   remote node ──LXMF(content = Params frame)──▶ ┌──────────────┐
//!                                                 │   App<D>     │ ──▶ ConfigStore (flash)
//!   remote node ◀──LXMF(content = Report frame)── │  Device D    │
//!                                                 └──────┬───────┘
//!                                                        │ step(input) → actuate
//!                                             Sensor ───▶│◀─── Actuator
//! ```
//!
//! # Layers
//!
//! | Layer | Module | Who writes it |
//! |---|---|---|
//! | Hardware traits — sensor, actuator, config store, clock, log | [`hal`] | the per-MCU wrapper |
//! | Wire frame helper — magic, version, CRC32 | [`frame`] | this crate |
//! | The device contract — `Params` in, `Report` out, `step` in between | [`device`] | the device author |
//! | The app loop — load/apply/persist config, sample cadence, status cadence | [`app`] | this crate |
//! | LXMF carrier — params out of a message, report into one | [`lxmf`] | this crate |
//! | A worked device — on/off thermostat with hysteresis | [`thermostat`] | example |
//!
//! The crate is `no_std + alloc` and has no I/O of its own: it hands out
//! byte vectors and takes byte slices. The transport that moves them
//! (NodeCore + a radio) stays in the wrapper, where it already lives
//! for every other LNode role.
//!
//! # What a wrapper does
//!
//! 1. Implement [`hal::Sensor`], [`hal::Actuator`], [`hal::ConfigStore`],
//!    [`hal::Clock`] over the board's peripherals (see
//!    `leviculum-esp/src/automation.rs` for the ESP32-S3 set).
//! 2. `let mut app = App::boot(device, &mut store, &clock);`
//! 3. In the main loop: `app.tick(&mut sensor, &mut actuator, &clock)`
//!    and, when it returns a report, hand `app.report_message(..)` to
//!    the mesh. On an inbound delivery for our destination, call
//!    `app.on_message(..)` and persist if it says so.
//!
//! Nothing in steps 2–3 names a pin, a flash driver, or a radio.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod device;
pub mod frame;
pub mod hal;
pub mod lxmf;
pub mod thermostat;

pub use app::{App, Inbound, Tick};
pub use device::{Device, Params, Report};
pub use frame::FrameError;
