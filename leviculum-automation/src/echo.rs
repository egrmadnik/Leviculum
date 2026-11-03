//! `echo` — the smallest possible device: a loopback.
//!
//! The remote puts up to [`ECHO_MAX`] arbitrary bytes into an `ECHC`
//! params frame; the board stores them and the next `ECHS` report
//! carries the same bytes back. Send "ping", get "ping" — the whole
//! LXMF round trip (signature, params decode, report encode, delivery,
//! target learning) exercised with a payload the caller chooses, which
//! is exactly what a link-bench node is for.
//!
//! There is nothing to sample and nothing to drive: `Input = ()`,
//! `Output = ()`, and the app ticks against [`crate::app::NoIo`].
//!
//! ```text
//!   remote ──ECHC(len, payload)──▶ App<Echo>::on_message → apply
//!   remote ◀──ECHS(len, payload, applied, uptime)── tick → report
//! ```

use crate::device::{Device, Params, Report};
use crate::frame::{Buf, Cursor, FrameError};

/// Most bytes the loopback carries — keeps the whole LXMF message
/// comfortably inside one opportunistic packet.
pub const ECHO_MAX: usize = 32;

/// `ECHC` v1 — 33-byte payload: the bytes to echo back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoParams {
    /// How many bytes of `payload` are real — the report echoes only
    /// `payload[..len]`. `valid` refuses `len > ECHO_MAX`.
    pub len: u8,
    pub payload: [u8; ECHO_MAX],
}

impl EchoParams {
    /// Convenience constructor — `EchoParams::of(b"ping")`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut payload = [0u8; ECHO_MAX];
        let n = bytes.len().min(ECHO_MAX);
        payload[..n].copy_from_slice(&bytes[..n]);
        Self {
            len: n as u8,
            payload,
        }
    }

    /// The real bytes, `payload[..len]`.
    pub fn content(&self) -> &[u8] {
        &self.payload[..self.len as usize]
    }
}

impl Params for EchoParams {
    const MAGIC: [u8; 4] = *b"ECHC";
    const VERSION: u8 = 1;
    const DEFAULT: Self = Self {
        len: 0,
        payload: [0; ECHO_MAX],
    };

    fn write(&self, out: &mut Buf<'_>) {
        out.u8(self.len);
        out.bytes(&self.payload);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        let len = c.u8()?;
        let mut payload = [0u8; ECHO_MAX];
        payload.copy_from_slice(c.bytes(ECHO_MAX)?);
        Ok(Self { len, payload })
    }

    fn valid(&self) -> bool {
        self.len as usize <= ECHO_MAX
    }

    /// The echo is a responder, not a sensor — there is no inherent
    /// cadence. `step` still runs (it is what produces the report), so
    /// a small sample period bounds reply latency.
    fn sample_ms(&self) -> u32 {
        1_000
    }

    /// Every applied params set wants its echo out promptly.
    fn report_every(&self) -> u32 {
        1
    }
}

/// `ECHS` v1 — 41-byte payload: the echo plus liveness counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoReport {
    /// Echoed length — mirrors `EchoParams::len`.
    pub len: u8,
    /// The echoed bytes (only `payload[..len]` are real).
    pub payload: [u8; ECHO_MAX],
    /// How many times params were applied since boot — the boot-time
    /// default/restore counts too, so a remote's first message reports
    /// `applied >= 2`. A cheap monotonic proof the reply is fresh, not
    /// replayed.
    pub applied: u32,
    pub uptime_s: u32,
}

impl EchoReport {
    /// The echoed bytes, `payload[..len]`.
    pub fn content(&self) -> &[u8] {
        let n = (self.len as usize).min(ECHO_MAX);
        &self.payload[..n]
    }
}

impl Report for EchoReport {
    const MAGIC: [u8; 4] = *b"ECHS";
    const VERSION: u8 = 1;

    fn write(&self, out: &mut Buf<'_>) {
        out.u8(self.len);
        out.bytes(&self.payload);
        out.u32(self.applied);
        out.u32(self.uptime_s);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        let len = c.u8()?;
        let mut payload = [0u8; ECHO_MAX];
        payload.copy_from_slice(c.bytes(ECHO_MAX)?);
        Ok(Self {
            len,
            payload,
            applied: c.u32()?,
            uptime_s: c.u32()?,
        })
    }
}

/// The device: last params in, same bytes out.
pub struct Echo {
    params: EchoParams,
    applied: u32,
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

impl Echo {
    pub fn new() -> Self {
        Self {
            params: EchoParams::DEFAULT,
            applied: 0,
        }
    }
}

impl Device for Echo {
    type Params = EchoParams;
    type Report = EchoReport;
    type Input = ();
    type Output = ();

    fn params(&self) -> &EchoParams {
        &self.params
    }

    fn apply(&mut self, params: EchoParams, _now_ms: u64) {
        self.params = params;
        self.applied = self.applied.saturating_add(1);
    }

    fn step(&mut self, _input: (), _now_ms: u64) {}

    fn report(&self, now_ms: u64) -> EchoReport {
        EchoReport {
            len: self.params.len,
            payload: self.params.payload,
            applied: self.applied,
            uptime_s: (now_ms / 1000) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_frame_roundtrip_and_validity() {
        let p = EchoParams::of(b"hello mesh");
        assert_eq!(EchoParams::decode(&p.encode()).unwrap(), p);
        assert_eq!(p.content(), b"hello mesh");
    }

    #[test]
    fn report_frame_roundtrip() {
        let r = EchoReport {
            len: 3,
            payload: *b"abc\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
            applied: 7,
            uptime_s: 60,
        };
        let got = EchoReport::decode(&r.encode()).unwrap();
        assert_eq!(got, r);
        assert_eq!(got.content(), b"abc");
    }

    #[test]
    fn apply_then_report_echoes_and_counts() {
        let mut d = Echo::new();
        d.apply(EchoParams::of(b"ping"), 0);
        let r = d.report(5_000);
        assert_eq!(r.content(), b"ping");
        assert_eq!(r.applied, 1);
        d.apply(EchoParams::of(b"pong"), 0);
        let r = d.report(6_000);
        assert_eq!(r.content(), b"pong");
        assert_eq!(r.applied, 2);
    }
}
