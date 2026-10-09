//! PID controller driving an on/off actuator — a valve, a heater relay,
//! anything binary — by **time-proportioning**: the continuous output
//! 0.0..1.0 becomes the fraction of a fixed window the actuator is on.
//!
//! The controller is ordinary (proportional + integral + derivative on
//! measurement, anti-windup by conditional integration); the shape that
//! matters is the wire: [`Config`] and [`Status`] each have one binary
//! frame that serves both flash persistence and the mesh payload a
//! remote node uses to tune or read the loop.
//!
//! Frame layouts (little-endian throughout):
//!
//! ```text
//! PIDC v1 — 34 bytes, the configuration frame
//!   0..3    "PIDC" magic
//!   4       version = 1
//!   5       flags (reserved, 0)
//!   6..9    kp f32      10..13  ki f32      14..17  kd f32
//!   18..21  setpoint_c f32
//!   22..25  sample_ms u32   26..29  window_ms u32
//!   30..33  crc32 (IEEE) over bytes 0..30
//!
//! PIDS v1 — 20 bytes, the status report
//!   0..3    "PIDS"
//!   4       version = 1
//!   5       flags: bit0 = actuator on, bit1 = sensor lost
//!   6..7    temperature °C ×100 i16
//!   8..9    setpoint °C ×100 i16
//!   10..11  duty ×1000 u16
//!   12..15  uptime_s u32
//!   16..19  crc32 over bytes 0..16
//! ```

use core::marker::PhantomData;

/// Why a frame failed to decode — bad magic, version, length, or CRC.
/// Deliberately not split further: every cause means "don't trust any
/// of it", and callers only ever refuse.
#[derive(Debug)]
pub struct FrameError;

/// Flash-slot magic + version, shared by the frame codec and the store.
pub const CONFIG_MAGIC: &[u8; 4] = b"PIDC";
pub const STATUS_MAGIC: &[u8; 4] = b"PIDS";
pub const FRAME_VERSION: u8 = 1;
pub const CONFIG_FRAME_LEN: usize = 34;
pub const STATUS_FRAME_LEN: usize = 20;

/// Everything a remote node may set — one struct for wire, flash, and
/// the live loop.
#[derive(Clone, Copy)]
pub struct Config {
    /// °C the loop drives toward.
    pub setpoint_c: f32,
    pub kp: f32,
    /// Per second of error.
    pub ki: f32,
    /// Per °C/s of measurement change.
    pub kd: f32,
    /// How often the temperature is sampled and the duty recomputed.
    pub sample_ms: u32,
    /// The time-proportioning window the duty plays out over.
    pub window_ms: u32,
}

impl Config {
    /// A sane default for a thermal loop: 22 °C, gentle integral, a
    /// 2 s sample, a 10 s valve window.
    pub const DEFAULT: Self = Self {
        setpoint_c: 22.0,
        kp: 4.0,
        ki: 0.05,
        kd: 0.0,
        sample_ms: 2_000,
        window_ms: 10_000,
    };

    /// Decode a `PIDC` frame. `Err` on bad magic, version, length, or
    /// CRC — a corrupt frame is a refusal, never a half-applied config.
    pub fn decode(frame: &[u8]) -> Result<Self, FrameError> {
        if frame.len() != CONFIG_FRAME_LEN
            || &frame[..4] != CONFIG_MAGIC
            || frame[4] != FRAME_VERSION
            || crc32(&frame[..30]) != u32::from_le_bytes(frame[30..34].try_into().unwrap())
        {
            return Err(FrameError);
        }
        Ok(Self {
            kp: f32::from_le_bytes(frame[6..10].try_into().unwrap()),
            ki: f32::from_le_bytes(frame[10..14].try_into().unwrap()),
            kd: f32::from_le_bytes(frame[14..18].try_into().unwrap()),
            setpoint_c: f32::from_le_bytes(frame[18..22].try_into().unwrap()),
            sample_ms: u32::from_le_bytes(frame[22..26].try_into().unwrap()),
            window_ms: u32::from_le_bytes(frame[26..30].try_into().unwrap()),
        })
    }

    /// The inverse — what goes into flash and over the wire.
    pub fn encode(&self) -> [u8; CONFIG_FRAME_LEN] {
        let mut f = [0u8; CONFIG_FRAME_LEN];
        f[..4].copy_from_slice(CONFIG_MAGIC);
        f[4] = FRAME_VERSION;
        f[6..10].copy_from_slice(&self.kp.to_le_bytes());
        f[10..14].copy_from_slice(&self.ki.to_le_bytes());
        f[14..18].copy_from_slice(&self.kd.to_le_bytes());
        f[18..22].copy_from_slice(&self.setpoint_c.to_le_bytes());
        f[22..26].copy_from_slice(&self.sample_ms.to_le_bytes());
        f[26..30].copy_from_slice(&self.window_ms.to_le_bytes());
        let crc = crc32(&f[..30]);
        f[30..34].copy_from_slice(&crc.to_le_bytes());
        f
    }
}

/// What the node reports back — the frame a remote node receives, one
/// per reporting tick.
#[derive(Clone, Copy)]
pub struct Status {
    /// Last good measurement °C; stale while `sensor_lost` is set.
    pub temperature_c: f32,
    pub setpoint_c: f32,
    /// 0.0..1.0 — the window fraction the valve is on.
    pub duty: f32,
    /// The actuator's live level this instant.
    pub output_on: bool,
    /// The sensor stopped answering: CRC fails or no presence pulse.
    pub sensor_lost: bool,
    pub uptime_s: u32,
}

impl Status {
    pub fn encode(&self) -> [u8; STATUS_FRAME_LEN] {
        let mut f = [0u8; STATUS_FRAME_LEN];
        f[..4].copy_from_slice(STATUS_MAGIC);
        f[4] = FRAME_VERSION;
        f[5] = (self.output_on as u8) | (self.sensor_lost as u8) << 1;
        f[6..8].copy_from_slice(&((self.temperature_c * 100.0) as i16).to_le_bytes());
        f[8..10].copy_from_slice(&((self.setpoint_c * 100.0) as i16).to_le_bytes());
        f[10..12].copy_from_slice(&((self.duty * 1000.0) as u16).to_le_bytes());
        f[12..16].copy_from_slice(&self.uptime_s.to_le_bytes());
        let crc = crc32(&f[..16]);
        f[16..20].copy_from_slice(&crc.to_le_bytes());
        f
    }

    pub fn decode(frame: &[u8]) -> Result<Self, FrameError> {
        if frame.len() != STATUS_FRAME_LEN
            || &frame[..4] != STATUS_MAGIC
            || frame[4] != FRAME_VERSION
            || crc32(&frame[..16]) != u32::from_le_bytes(frame[16..20].try_into().unwrap())
        {
            return Err(FrameError);
        }
        Ok(Self {
            temperature_c: i16::from_le_bytes(frame[6..8].try_into().unwrap()) as f32 / 100.0,
            setpoint_c: i16::from_le_bytes(frame[8..10].try_into().unwrap()) as f32 / 100.0,
            duty: u16::from_le_bytes(frame[10..12].try_into().unwrap()) as f32 / 1000.0,
            output_on: frame[5] & 1 != 0,
            sensor_lost: frame[5] & 2 != 0,
            uptime_s: u32::from_le_bytes(frame[12..16].try_into().unwrap()),
        })
    }
}

/// The loop itself.
pub struct Pid {
    cfg: Config,
    /// Accumulated error×seconds — the I term, clamped so a saturated
    /// output stops winding it further (conditional integration).
    integral: f32,
    /// Previous measurement — derivative on measurement, so a setpoint
    /// step does not produce a derivative kick.
    prev_t: Option<f32>,
    /// Millis when the current window opened — the valve duty plays out
    /// inside it.
    window_start_ms: u64,
    _pd: PhantomData<()>,
}

impl Pid {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            integral: 0.0,
            prev_t: None,
            window_start_ms: 0,
            _pd: PhantomData,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// A new config resets the loop's memory: integral and derivative
    /// history belong to the constants they were earned under, and a
    /// remote re-tune must not inherit them. The window restarts too.
    pub fn set_config(&mut self, cfg: Config, now_ms: u64) {
        self.cfg = cfg;
        self.integral = 0.0;
        self.prev_t = None;
        self.window_start_ms = now_ms;
    }

    /// One measurement → the new duty, 0.0..1.0.
    ///
    /// `dt` is the caller's real elapsed seconds; trusting the caller
    /// (rather than a clock inside) keeps the loop testable.
    pub fn update(&mut self, t_c: f32, dt_s: f32) -> f32 {
        let e = self.cfg.setpoint_c - t_c;

        // D on measurement, sign flipped: d(error)/dt = -d(meas)/dt.
        let deriv = match self.prev_t {
            Some(prev) if dt_s > 0.0 => -(t_c - prev) / dt_s,
            _ => 0.0,
        };
        self.prev_t = Some(t_c);

        // Conditional integration: accumulate only while the output is
        // unsaturated, or while the error would pull it back. A held
        // valve with a wound-up integrator overshoots on release.
        let trial_i = self.integral + e * dt_s;
        let p = self.cfg.kp * e;
        let d = self.cfg.kd * deriv;
        let trial_out = p + self.cfg.ki * trial_i + d;
        if (0.0..=1.0).contains(&trial_out) || e * self.integral < 0.0 {
            self.integral = trial_i;
        }
        let out = p + self.cfg.ki * self.integral + d;
        out.clamp(0.0, 1.0)
    }

    /// What the valve does *now*: the duty becomes an on-fraction of
    /// `window_ms`. `duty` comes from the last [`update`](Self::update).
    pub fn valve_on(&mut self, duty: f32, now_ms: u64) -> bool {
        let window = self.cfg.window_ms.max(1) as u64;
        let mut elapsed = now_ms - self.window_start_ms;
        if elapsed >= window {
            // Advance whole windows, not to now — a stalled tick keeps
            // the phase instead of silently lengthening one window.
            self.window_start_ms += window * (elapsed / window);
            elapsed %= window;
        }
        (elapsed as f32) < duty * window as f32
    }
}

/// CRC-32 (IEEE, reflected poly 0xEDB88320) — table-less. `pub(crate)`
/// because the flash store CRCs the identity frame with the same one.
pub(crate) fn crc32_pub(bytes: &[u8]) -> u32 {
    crc32(bytes)
}

/// CRC-32 (IEEE, reflected poly 0xEDB88320) — table-less.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
