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
    const PAYLOAD_LEN: usize = 1 + ECHO_MAX;
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

    /// Report only when new params are applied (`0` = on-demand only,
    /// never spam periodic reports when idle).
    fn report_every(&self) -> u32 {
        0
    }
}

/// `ECHS` v1 — 45-byte payload: the echo plus liveness counters and radio signal.
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
    /// Last received LoRa RSSI in dBm (`i16::MIN` when not received over radio).
    pub rssi_dbm: i16,
    /// Last received LoRa SNR in dB (`i16::MIN` when not received over radio).
    pub snr_db: i16,
}

impl EchoReport {
    /// The echoed bytes, `payload[..len]`.
    pub fn content(&self) -> &[u8] {
        let n = (self.len as usize).min(ECHO_MAX);
        &self.payload[..n]
    }

    /// `(rssi_dbm, snr_db)` if a radio signal measurement is present.
    pub fn signal(&self) -> Option<(i16, i16)> {
        (self.rssi_dbm != i16::MIN).then_some((self.rssi_dbm, self.snr_db))
    }
}

impl Report for EchoReport {
    const MAGIC: [u8; 4] = *b"ECHS";
    const VERSION: u8 = 1;
    const PAYLOAD_LEN: usize = 1 + ECHO_MAX + 4 + 4 + 2 + 2;

    fn write(&self, out: &mut Buf<'_>) {
        out.u8(self.len);
        out.bytes(&self.payload);
        out.u32(self.applied);
        out.u32(self.uptime_s);
        out.i16(self.rssi_dbm);
        out.i16(self.snr_db);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        let len = c.u8()?;
        let mut payload = [0u8; ECHO_MAX];
        payload.copy_from_slice(c.bytes(ECHO_MAX)?);
        let applied = c.u32()?;
        let uptime_s = c.u32()?;
        let (rssi_dbm, snr_db) = if c.remaining() >= 4 {
            (c.i16()?, c.i16()?)
        } else {
            (i16::MIN, i16::MIN)
        };
        Ok(Self {
            len,
            payload,
            applied,
            uptime_s,
            rssi_dbm,
            snr_db,
        })
    }
}

/// The device: last params in, same bytes out.
pub struct Echo {
    params: EchoParams,
    applied: u32,
    last_rssi_dbm: i16,
    last_snr_db: i16,
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
            last_rssi_dbm: i16::MIN,
            last_snr_db: i16::MIN,
        }
    }

    /// Record the radio signal strength (RSSI in dBm, SNR in dB) of the
    /// last received packet (`i16::MIN` when received over non-radio transport).
    pub fn set_last_signal(&mut self, rssi_dbm: i16, snr_db: i16) {
        self.last_rssi_dbm = rssi_dbm;
        self.last_snr_db = snr_db;
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
            rssi_dbm: self.last_rssi_dbm,
            snr_db: self.last_snr_db,
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
            rssi_dbm: -52,
            snr_db: 9,
        };
        let got = EchoReport::decode(&r.encode()).unwrap();
        assert_eq!(got, r);
        assert_eq!(got.content(), b"abc");
        assert_eq!(got.signal(), Some((-52, 9)));
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
