//! `io-test` — a multi-channel I/O exerciser device.
//!
//! The smallest device that exercises the *multi-channel* HAL traits
//! ([`DigitalIn`], [`DigitalOut`], [`AnalogIn`]) the way the thermostat
//! exercises [`Sensor`]/[`Actuator`]: a bank of digital inputs, a bank
//! of digital outputs, and a row of ADC channels, with a remote-set
//! mode and cadence.
//!
//! Two operating modes, selected by `flags`:
//!
//! - **Mirror** (`FLAG_MIRROR`): `dout = din` every cycle — the classic
//!   bench loopback. Wire DI*n* to a switch, watch DO*n* and the report
//!   follow it.
//! - **Remote** (`dout_mask`): the remote node writes the output mask
//!   directly; the inputs are still sampled and reported.
//!
//! [`IoBus`] bundles the three HAL traits into the [`Io`] impl the
//! app ticks against, so a wrapper implements `DigitalIn`/
//! `DigitalOut`/`AnalogIn` once and gets the whole device.
//!
//! ```text
//!   switches ─▶ DigitalIn ─┐
//!   ADC pots ─▶ AnalogIn  ─┤ IoBus ─▶ App<IoTest>::tick ─▶ DigitalOut ─▶ relays/LEDs
//!                          └             │                       ▲
//!                    IOTS report ◀── report()           step() ──┘
//!                    IOTC params ─▶ apply()
//! ```

use crate::app::Io;
use crate::device::{Device, Params, Report};
use crate::frame::{Buf, Cursor, FrameError};
use crate::hal::{AnalogIn, DigitalIn, DigitalOut};

/// Digital input channels the report carries.
pub const DI_CHANNELS: usize = 8;
/// Digital output channels `IoCommand` carries.
pub const DO_CHANNELS: usize = 8;
/// ADC channels sampled per cycle.
pub const AI_CHANNELS: usize = 4;

/// `flags` bit: outputs mirror the inputs instead of `dout_mask`.
pub const FLAG_MIRROR: u8 = 0x01;

/// `flags` bit in [`IoTestReport`] — set when the digital bank read
/// produced a value this cycle (any channel). Cleared when every DI
/// channel returned `None`.
pub const FLAG_DIN_VALID: u8 = 0x02;

/// `IOTC` v1 — 8 bytes of payload.
///
/// The remote's handle on the device: which mode, what mask, how fast.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IoTestParams {
    /// Bit 0 ([`FLAG_MIRROR`]): outputs follow inputs. Clear: outputs
    /// follow `dout_mask`.
    pub flags: u8,
    /// Manual output mask when `FLAG_MIRROR` is clear. Bit *n* = DO*n*.
    pub dout_mask: u8,
    /// Poll cadence — how often DI/AI are sampled and DO driven.
    pub sample_ms: u32,
    /// Report every N samples.
    pub report_every: u16,
}

impl Params for IoTestParams {
    const MAGIC: [u8; 4] = *b"IOTC";
    const VERSION: u8 = 1;
    const DEFAULT: Self = Self {
        flags: FLAG_MIRROR,
        dout_mask: 0,
        sample_ms: 500,
        report_every: 20,
    };

    fn write(&self, out: &mut Buf<'_>) {
        out.u8(self.flags);
        out.u8(self.dout_mask);
        out.u32(self.sample_ms);
        out.u16(self.report_every);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        Ok(Self {
            flags: c.u8()?,
            dout_mask: c.u8()?,
            sample_ms: c.u32()?,
            report_every: c.u16()?,
        })
    }

    fn valid(&self) -> bool {
        // Unknown flag bits reserved for later flags must be zero so a
        // v2 remote cannot silently program a v1 board.
        self.flags & !FLAG_MIRROR == 0
            && self.sample_ms >= 20
            && self.report_every >= 1
    }

    fn sample_ms(&self) -> u32 {
        self.sample_ms
    }

    fn report_every(&self) -> u32 {
        self.report_every as u32
    }
}

/// One cycle's input snapshot — `Device::Input` for [`IoTest`].
///
/// `din` is the DI bitmask (`None` when every channel failed to read),
/// `adc` one entry per channel (`None` per failed/unpopulated read).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IoSample {
    pub din: Option<u8>,
    pub adc: [Option<u16>; AI_CHANNELS],
}

impl IoSample {
    /// Any channel produced a reading.
    fn any_present(&self) -> bool {
        self.din.is_some() || self.adc.iter().any(Option::is_some)
    }
}

/// One cycle's output command — `Device::Output` for [`IoTest`].
/// Bit *n* drives DO*n*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoCommand {
    pub dout: u8,
}

/// `IOTS` v1 — 15 bytes of payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IoTestReport {
    /// Last DI bitmask (0 when the bank never read).
    pub din: u8,
    /// Output mask currently driven.
    pub dout: u8,
    /// `FLAG_MIRROR` echoes the mode; `FLAG_DIN_VALID` says `din` is a
    /// reading, not the power-on default.
    pub flags: u8,
    /// Raw ADC counts per channel; `u16::MAX` = no reading.
    pub adc: [u16; AI_CHANNELS],
    pub uptime_s: u32,
}

impl Report for IoTestReport {
    const MAGIC: [u8; 4] = *b"IOTS";
    const VERSION: u8 = 1;

    fn write(&self, out: &mut Buf<'_>) {
        out.u8(self.din);
        out.u8(self.dout);
        out.u8(self.flags);
        for c in self.adc {
            out.u16(c);
        }
        out.u32(self.uptime_s);
    }

    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        let din = c.u8()?;
        let dout = c.u8()?;
        let flags = c.u8()?;
        let mut adc = [0u16; AI_CHANNELS];
        for slot in &mut adc {
            *slot = c.u16()?;
        }
        Ok(Self {
            din,
            dout,
            flags,
            adc,
            uptime_s: c.u32()?,
        })
    }
}

/// The device: params, last sample, current output mask.
pub struct IoTest {
    params: IoTestParams,
    din: Option<u8>,
    adc: [Option<u16>; AI_CHANNELS],
    dout: u8,
}

impl Default for IoTest {
    fn default() -> Self {
        Self::new()
    }
}

impl IoTest {
    pub fn new() -> Self {
        Self {
            params: IoTestParams::DEFAULT,
            din: None,
            adc: [None; AI_CHANNELS],
            dout: 0,
        }
    }
}

impl Device for IoTest {
    type Params = IoTestParams;
    type Report = IoTestReport;
    type Input = IoSample;
    type Output = IoCommand;

    fn params(&self) -> &IoTestParams {
        &self.params
    }

    fn apply(&mut self, params: IoTestParams, _now_ms: u64) {
        self.params = params;
        // The output is deliberately *not* slammed here: whichever mask
        // applies next (mirror or remote) is re-derived on the very next
        // step, and holding the physical state for that gap is safer
        // than a 0-pulse on every re-configure.
    }

    fn input_valid(input: &IoSample) -> bool {
        input.any_present()
    }

    fn step(&mut self, input: IoSample, _now_ms: u64) -> IoCommand {
        self.din = input.din;
        self.adc = input.adc;
        self.dout = if self.params.flags & FLAG_MIRROR != 0 {
            input.din.unwrap_or(0)
        } else {
            self.params.dout_mask
        };
        IoCommand { dout: self.dout }
    }

    fn report(&self, now_ms: u64) -> IoTestReport {
        let mut adc = [u16::MAX; AI_CHANNELS];
        for (slot, reading) in adc.iter_mut().zip(self.adc.iter()) {
            if let Some(c) = reading {
                *slot = *c;
            }
        }
        IoTestReport {
            din: self.din.unwrap_or(0),
            dout: self.dout,
            flags: (self.params.flags & FLAG_MIRROR) | (self.din.is_some() as u8) << 1,
            adc,
            uptime_s: (now_ms / 1000) as u32,
        }
    }
}

/// Bundles the three bank HAL traits into the [`Io`] impl
/// [`crate::app::App<IoTest>`] ticks against — the only glue a wrapper
/// writes for this device.
///
/// ```ignore
/// let mut io = IoBus {
///     din: &mut di_bank,
///     dout: &mut do_bank,
///     adc: &mut adc_bank,
/// };
/// app.tick(&mut io, &clock, &mut log);
/// ```
pub struct IoBus<'a, DI: DigitalIn, DO: DigitalOut, AI: AnalogIn> {
    pub din: &'a mut DI,
    pub dout: &'a mut DO,
    pub adc: &'a mut AI,
}

impl<DI, DO, AI> Io<IoTest> for IoBus<'_, DI, DO, AI>
where
    DI: DigitalIn,
    DO: DigitalOut,
    AI: AnalogIn,
{
    fn sample(&mut self) -> IoSample {
        let mut din = 0u8;
        let mut any = false;
        for ch in 0..DI_CHANNELS as u8 {
            if let Some(level) = self.din.read(ch) {
                any = true;
                if level {
                    din |= 1 << ch;
                }
            }
        }
        let mut adc = [None; AI_CHANNELS];
        for (ch, slot) in adc.iter_mut().enumerate() {
            *slot = self.adc.read_counts(ch as u8);
        }
        IoSample {
            din: any.then_some(din),
            adc,
        }
    }

    fn drive(&mut self, out: &IoCommand) {
        for ch in 0..DO_CHANNELS as u8 {
            self.dout.set(ch, out.dout & (1 << ch) != 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_frame_roundtrip_and_validity() {
        let p = IoTestParams::DEFAULT;
        assert_eq!(IoTestParams::decode(&p.encode()).unwrap(), p);
        let too_fast = IoTestParams {
            sample_ms: 10,
            ..p
        };
        assert_eq!(
            IoTestParams::decode(&too_fast.encode()),
            Err(FrameError::Invalid)
        );
        let stray_flags = IoTestParams {
            flags: 0x80,
            ..p
        };
        assert_eq!(
            IoTestParams::decode(&stray_flags.encode()),
            Err(FrameError::Invalid)
        );
    }

    #[test]
    fn report_frame_roundtrip() {
        let r = IoTestReport {
            din: 0b1010,
            dout: 0b1010,
            flags: FLAG_MIRROR | FLAG_DIN_VALID,
            adc: [0, 1024, u16::MAX, 4095],
            uptime_s: 3600,
        };
        assert_eq!(IoTestReport::decode(&r.encode()).unwrap(), r);
    }

    #[test]
    fn mirror_mode_copies_din_to_dout() {
        let mut d = IoTest::new(); // DEFAULT: mirror on
        let out = d.step(
            IoSample {
                din: Some(0b0101),
                adc: [None; 4],
            },
            0,
        );
        assert_eq!(out.dout, 0b0101);
    }

    #[test]
    fn remote_mode_uses_dout_mask() {
        let mut d = IoTest::new();
        d.apply(
            IoTestParams {
                flags: 0,
                dout_mask: 0b1100,
                ..IoTestParams::DEFAULT
            },
            0,
        );
        // din must not leak into dout in remote mode.
        let out = d.step(
            IoSample {
                din: Some(0b0001),
                adc: [Some(100); 4],
            },
            0,
        );
        assert_eq!(out.dout, 0b1100);
    }

    #[test]
    fn no_reads_marks_input_invalid_and_din_absent() {
        let mut d = IoTest::new();
        let input = IoSample {
            din: None,
            adc: [None; 4],
        };
        assert!(!IoTest::input_valid(&input));
        d.step(input, 0);
        let r = d.report(1000);
        assert_eq!(r.flags & FLAG_DIN_VALID, 0);
        assert_eq!(r.adc, [u16::MAX; 4]);
    }

    struct FakeDi(u8);
    impl DigitalIn for FakeDi {
        fn read(&mut self, ch: u8) -> Option<bool> {
            (ch < 8).then(|| self.0 >> ch & 1 == 1)
        }
    }
    struct FakeDo(u8);
    impl DigitalOut for FakeDo {
        fn set(&mut self, ch: u8, on: bool) {
            if on {
                self.0 |= 1 << ch;
            } else {
                self.0 &= !(1 << ch);
            }
        }
    }
    struct FakeAi;
    impl AnalogIn for FakeAi {
        fn read_counts(&mut self, ch: u8) -> Option<u16> {
            (ch < 4).then_some(ch as u16 * 1000)
        }
    }

    #[test]
    fn io_bus_samples_and_drives_all_channels() {
        let mut di = FakeDi(0b0011);
        let mut do_ = FakeDo(0);
        let mut ai = FakeAi;
        let mut io = IoBus {
            din: &mut di,
            dout: &mut do_,
            adc: &mut ai,
        };
        let s = io.sample();
        assert_eq!(s.din, Some(0b0011));
        assert_eq!(s.adc, [Some(0), Some(1000), Some(2000), Some(3000)]);
        io.drive(&IoCommand { dout: 0b1010 });
        assert_eq!(do_.0, 0b1010);
    }
}
