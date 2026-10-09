//! The worked example: an on/off thermostat with hysteresis.
//!
//! Heater on below `setpoint - hysteresis`, off above `setpoint +
//! hysteresis`, hold in between. No reading → off. It is the smallest
//! device that exercises every part of the contract: params with a
//! validity rule, a report with a failsafe flag, state that `apply`
//! must reset.
//!
//! Replacing it with a PID, a pump scheduler or a counter is a new
//! file implementing the same three traits — `App` does not change.

use alloc::vec::Vec;

use crate::device::{Device, Output, Params, Report};
use crate::frame::{Cursor, FrameError};

/// `THRM` v1 — 14 bytes of payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThermostatParams {
    /// Target temperature.
    pub setpoint_c: f32,
    /// Half-width of the dead band.
    pub hysteresis_c: f32,
    /// Sensor/decision period.
    pub sample_ms: u32,
    /// Report every N samples.
    pub report_every: u16,
}

impl Params for ThermostatParams {
    const MAGIC: [u8; 4] = *b"THRM";
    const VERSION: u8 = 1;
    const DEFAULT: Self = Self {
        setpoint_c: 21.0,
        hysteresis_c: 0.5,
        sample_ms: 10_000,
        report_every: 6,
    };

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.setpoint_c.to_le_bytes());
        out.extend_from_slice(&self.hysteresis_c.to_le_bytes());
        out.extend_from_slice(&self.sample_ms.to_le_bytes());
        out.extend_from_slice(&self.report_every.to_le_bytes());
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        Ok(Self {
            setpoint_c: c.f32()?,
            hysteresis_c: c.f32()?,
            sample_ms: c.u32()?,
            report_every: c.u16()?,
        })
    }

    fn valid(&self) -> bool {
        self.setpoint_c.is_finite()
            && self.hysteresis_c.is_finite()
            && self.hysteresis_c >= 0.0
            && (-50.0..=150.0).contains(&self.setpoint_c)
            && self.sample_ms >= 500
            && self.report_every >= 1
    }

    fn sample_ms(&self) -> u32 {
        self.sample_ms
    }

    fn report_every(&self) -> u32 {
        self.report_every as u32
    }
}

/// `THRS` v1 — 12 bytes of payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThermostatReport {
    pub temperature_c: f32,
    pub setpoint_c: f32,
    pub on: bool,
    pub sensor_lost: bool,
    pub uptime_s: u32,
}

impl Report for ThermostatReport {
    const MAGIC: [u8; 4] = *b"THRS";
    const VERSION: u8 = 1;

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.temperature_c.to_le_bytes());
        out.extend_from_slice(&self.setpoint_c.to_le_bytes());
        out.push((self.on as u8) | ((self.sensor_lost as u8) << 1));
        out.extend_from_slice(&self.uptime_s.to_le_bytes()[..3]);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        let temperature_c = c.f32()?;
        let setpoint_c = c.f32()?;
        let flags = c.u8()?;
        let (a, b, d) = (c.u8()?, c.u8()?, c.u8()?);
        Ok(Self {
            temperature_c,
            setpoint_c,
            on: flags & 1 != 0,
            sensor_lost: flags & 2 != 0,
            uptime_s: u32::from_le_bytes([a, b, d, 0]),
        })
    }
}

pub struct Thermostat {
    params: ThermostatParams,
    on: bool,
    last_temp: Option<f32>,
}

impl Default for Thermostat {
    fn default() -> Self {
        Self::new()
    }
}

impl Thermostat {
    pub fn new() -> Self {
        Self {
            params: ThermostatParams::DEFAULT,
            on: false,
            last_temp: None,
        }
    }
}

impl Device for Thermostat {
    type Params = ThermostatParams;
    type Report = ThermostatReport;

    fn params(&self) -> &ThermostatParams {
        &self.params
    }

    fn apply(&mut self, params: ThermostatParams, _now_ms: u64) {
        self.params = params;
        // A new setpoint re-decides from scratch; carrying `on` across
        // would let an old band hold the heater through a new one.
        self.on = false;
    }

    fn step(&mut self, input: Option<f32>, _now_ms: u64) -> Output {
        self.last_temp = input;
        let Some(t) = input else {
            self.on = false;
            return Output { on: false };
        };
        let lo = self.params.setpoint_c - self.params.hysteresis_c;
        let hi = self.params.setpoint_c + self.params.hysteresis_c;
        if t < lo {
            self.on = true;
        } else if t > hi {
            self.on = false;
        }
        Output { on: self.on }
    }

    fn report(&self, now_ms: u64) -> ThermostatReport {
        ThermostatReport {
            temperature_c: self.last_temp.unwrap_or(f32::NAN),
            setpoint_c: self.params.setpoint_c,
            on: self.on,
            sensor_lost: self.last_temp.is_none(),
            uptime_s: (now_ms / 1000) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_frame_roundtrip_and_validity() {
        let p = ThermostatParams::DEFAULT;
        let f = p.encode();
        assert_eq!(ThermostatParams::decode(&f).unwrap(), p);
        let bad = ThermostatParams { sample_ms: 10, ..p };
        assert_eq!(
            ThermostatParams::decode(&bad.encode()),
            Err(FrameError::Invalid)
        );
        let nan = ThermostatParams {
            setpoint_c: f32::NAN,
            ..p
        };
        assert_eq!(
            ThermostatParams::decode(&nan.encode()),
            Err(FrameError::Invalid)
        );
    }

    #[test]
    fn report_frame_roundtrip() {
        let r = ThermostatReport {
            temperature_c: 19.25,
            setpoint_c: 21.0,
            on: true,
            sensor_lost: false,
            uptime_s: 0x012345,
        };
        assert_eq!(ThermostatReport::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn hysteresis_band() {
        let mut d = Thermostat::new(); // 21 ± 0.5
        assert!(d.step(Some(20.0), 0).on);
        assert!(d.step(Some(21.0), 0).on, "inside band holds on");
        assert!(!d.step(Some(21.6), 0).on);
        assert!(!d.step(Some(21.0), 0).on, "inside band holds off");
        assert!(!d.step(None, 0).on, "no reading → off");
        assert!(d.report(5000).sensor_lost);
    }

    #[test]
    fn apply_resets_state() {
        let mut d = Thermostat::new();
        d.step(Some(10.0), 0);
        assert!(d.on);
        d.apply(ThermostatParams::DEFAULT, 0);
        assert!(!d.on);
    }
}
