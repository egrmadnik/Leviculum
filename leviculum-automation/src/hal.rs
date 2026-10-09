//! The hardware the device needs, as traits — the whole MCU-specific
//! surface, so a wrapper is exactly these five impls.
//!
//! Each is synchronous and infallible-or-`Option`: the app loop is a
//! polled main loop on both targets, and a sensor that could not read is
//! an input the device must handle (`None` → failsafe), not an error the
//! loop must unwind.

use alloc::vec::Vec;

/// A measured quantity the device regulates on — temperature, pressure,
/// a level. One value per call; the unit is the device's business.
pub trait Sensor {
    /// `None` when there is no valid reading this cycle (sensor absent,
    /// CRC fail, not yet converted). The device decides what that means.
    fn read(&mut self) -> Option<f32>;
}

/// An on/off output — a relay, a valve, a heater contactor.
pub trait Actuator {
    fn set(&mut self, on: bool);
}

/// Where the device's parameters live across a reboot.
///
/// The app writes one opaque blob (the `Params` frame plus its report
/// target) and reads it back; the store does not know the layout. A
/// flash sector, an EEPROM page, a RAM buffer in a test — anything that
/// holds ~64 bytes.
pub trait ConfigStore {
    /// The last saved blob, or `None` on fresh/corrupt storage.
    fn load(&mut self) -> Option<Vec<u8>>;
    /// Persist the blob. `false` if the write failed — the app reports
    /// that and carries on with the config in RAM.
    fn save(&mut self, blob: &[u8]) -> bool;
}

/// Monotonic milliseconds since boot. Not a wall clock.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// A line sink for the device's diagnostics. The app logs state
/// transitions, not every sample.
pub trait Log {
    fn line(&mut self, tag: &str, args: core::fmt::Arguments<'_>);
}

/// A `Log` that drops everything — for a wrapper that has no port, or a
/// host test that does not care.
pub struct NoLog;

impl Log for NoLog {
    fn line(&mut self, _tag: &str, _args: core::fmt::Arguments<'_>) {}
}
