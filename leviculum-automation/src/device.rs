//! The contract a device fulfils. Three traits:
//!
//! - [`Params`] — what a remote node sets. Has a wire layout and a
//!   validity rule.
//! - [`Report`] — what the device says about itself. Wire layout only.
//! - [`Device`] — holds params, takes an input each cycle, decides the
//!   output, produces a report.
//!
//! The app loop ([`crate::app`]) is written against these and nothing
//! else, so a new device is a new type implementing them — the LXMF
//! path, the flash path, the cadence all come for free.
//!
//! `Device::Input` and `Device::Output` are associated types: a
//! thermostat samples `Option<f32>` and drives a single on/off
//! [`Output`]; an I/O panel samples a channel bitmask plus an ADC bank
//! and drives an output mask. The app does not know which — it asks
//! its [`crate::app::Io`] adaptor to produce one and consume the
//! other.

use alloc::vec::Vec;

use crate::frame::{self, Buf, Cursor, FrameError};

/// Parameters a remote node can set.
pub trait Params: Copy {
    /// Frame magic — four ASCII bytes naming the device, e.g. `b"THRM"`.
    const MAGIC: [u8; 4];
    /// Layout version; bump when `write`/`read` change.
    const VERSION: u8;

    /// Compiled-in defaults — what runs when flash is empty.
    const DEFAULT: Self;

    /// Serialise the fields (little-endian, fixed order).
    fn write(&self, out: &mut Buf<'_>);
    /// Deserialise the fields from a cursor over the payload.
    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError>;
    /// Refuse values that would misbehave (zero periods, NaN gains).
    fn valid(&self) -> bool;

    /// How often the device wants `step` called.
    fn sample_ms(&self) -> u32;

    /// Report every N samples. Default: every sample.
    fn report_every(&self) -> u32 {
        1
    }

    /// Full frame: envelope + fields.
    fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        self.write(&mut Buf::new(&mut payload));
        frame::encode(&Self::MAGIC, Self::VERSION, &payload)
    }

    /// Envelope check, field read, validity — any failure is a refusal,
    /// never a partial apply.
    fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let payload = frame::decode(bytes, &Self::MAGIC, Self::VERSION)?;
        let mut c = Cursor::new(payload);
        let p = Self::read(&mut c)?;
        if c.remaining() != 0 || !p.valid() {
            return Err(FrameError::Invalid);
        }
        Ok(p)
    }
}

/// What the device reports. Symmetric with [`Params`] so a host-side
/// consumer decodes it with the same crate.
pub trait Report: Sized {
    const MAGIC: [u8; 4];
    const VERSION: u8;

    fn write(&self, out: &mut Buf<'_>);
    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError>;

    fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        self.write(&mut Buf::new(&mut payload));
        frame::encode(&Self::MAGIC, Self::VERSION, &payload)
    }

    fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let payload = frame::decode(bytes, &Self::MAGIC, Self::VERSION)?;
        let mut c = Cursor::new(payload);
        let r = Self::read(&mut c)?;
        if c.remaining() != 0 {
            return Err(FrameError::Invalid);
        }
        Ok(r)
    }
}

/// One control cycle's decision for a single-channel on/off device —
/// the shape a thermostat, a relay driver or a valve controller
/// produces. Multi-channel devices use their own `Output` type (e.g.
/// an output bitmask); this one stays for the common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    /// Drive the actuator on?
    pub on: bool,
}

/// The device itself.
pub trait Device {
    type Params: Params;
    type Report: Report;
    /// What one sample looks like: `Option<f32>` for a regulator,
    /// a snapshot of an I/O bank for a panel.
    type Input: Copy;
    /// What one actuation command looks like: [`Output`] for a single
    /// channel, a bitmask for a bank.
    type Output;

    /// The running parameters.
    fn params(&self) -> &Self::Params;
    /// Replace them — already validated by [`Params::decode`]. Reset
    /// whatever state the old tune accumulated (integrators, timers).
    fn apply(&mut self, params: Self::Params, now_ms: u64);

    /// Whether a sampled input is usable. `false` logs a failsafe line
    /// in the app; the device still sees the value and decides what
    /// "no valid input" means for the output. Default: every sample is
    /// good — right for devices whose input has no absent case.
    fn input_valid(input: &Self::Input) -> bool {
        let _ = input;
        true
    }

    /// One cycle: the input, the clock, the decision. A device whose
    /// input can be absent must fail safe on it.
    fn step(&mut self, input: Self::Input, now_ms: u64) -> Self::Output;

    /// Between samples the output may still need updating (a
    /// time-proportioned window). Default: hold the last decision.
    fn hold(&mut self, _now_ms: u64) -> Option<Self::Output> {
        None
    }

    /// The status to report after a cycle.
    fn report(&self, now_ms: u64) -> Self::Report;
}
