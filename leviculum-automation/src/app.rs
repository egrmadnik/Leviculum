//! The device-agnostic main loop: boot from flash, sample on cadence,
//! actuate, report, accept remote params.
//!
//! `App<D>` owns the device and the report target and nothing else.
//! It is driven, not driving: the wrapper calls [`App::tick`] from its
//! main loop and [`App::on_message`] when the mesh delivers something,
//! and acts on what comes back. That keeps the radio, the node and the
//! peripherals in the wrapper — the one place they already are.
//!
//! # Sampling and driving — [`Io`]
//!
//! The app does not know the device's input and output types; the
//! wrapper supplies them through [`Io`], one impl per board+device
//! pair:
//!
//! ```ignore
//! // A regulator: the (Sensor, Actuator) pair is already an Io impl —
//! app.tick(&mut (&mut sensor, &mut actuator), &clock, &mut log);
//!
//! // A panel: IoBus bundles the three bank traits —
//! app.tick(&mut IoBus { din: &mut di, dout: &mut do_, adc: &mut ai }, ..);
//! ```
//!
//! # Flash blob
//!
//! ```text
//! +-----------------------+---------------+------+
//! | Params frame (n B)    | target (16 B) | flag |
//! +-----------------------+---------------+------+
//! ```
//!
//! `flag` = 0x54 when `target` is real. The Params frame carries its own
//! CRC, so a corrupt sector reads as "defaults, no target".
//!
//! # Persistence is the wrapper's call
//!
//! [`App::on_message`] returns `Inbound::Applied { blob, .. }` and does
//! **not** write the store itself. On a build where flash writes go
//! through a task (the nRF SoftDevice owns NVMC), the wrapper hands
//! `blob` to that task; on a bare build it calls `store.save(&blob)`.
//! One API serves both.

use alloc::vec::Vec;

use leviculum_core::Identity;
use leviculum_lxmf::message::Verification;

use crate::device::{Device, Output, Params};
use crate::hal::{Actuator, Clock, ConfigStore, Log, Sensor};
use crate::lxmf;

const TARGET_PRESENT: u8 = 0x54;

/// Samples a device's input and applies its output.
///
/// One impl per board+device pair in the wrapper — the app asks for
/// `D::Input`, hands back `D::Output`, and everything between (GPIO
/// banks, 1-Wire probes, ADC channels) stays out of this crate.
pub trait Io<D: Device> {
    /// Produce one input sample for `Device::step`.
    fn sample(&mut self) -> D::Input;
    /// Apply one output command to the hardware.
    fn drive(&mut self, out: &D::Output);
}

/// A ([`Sensor`], [`Actuator`]) pair drives any single-channel
/// regulator — the shape every `Input = Option<f32>`,
/// `Output = Output` device has. The tuple is a temporary of
/// references, so nothing moves out of the wrapper's hands:
///
/// ```ignore
/// app.tick(&mut (&mut sensor, &mut actuator), &clock, &mut log);
/// ```
impl<D, S, A> Io<D> for (&mut S, &mut A)
where
    D: Device<Input = Option<f32>, Output = Output>,
    S: Sensor,
    A: Actuator,
{
    fn sample(&mut self) -> D::Input {
        self.0.read()
    }

    fn drive(&mut self, out: &D::Output) {
        self.1.set(out.on);
    }
}

/// An [`Io`] for devices with no inputs and no outputs — the
/// [`crate::echo`] loopback and its kin. `sample` yields `()` and
/// `drive` is a no-op; the cadence still runs, which is what produces
/// reports:
///
/// ```ignore
/// app.tick(&mut NoIo, &clock, &mut log);
/// ```
pub struct NoIo;

impl<D> Io<D> for NoIo
where
    D: Device<Input = (), Output = ()>,
{
    fn sample(&mut self) -> D::Input {}

    fn drive(&mut self, _out: &D::Output) {}
}

/// What a tick produced for the wrapper to act on.
pub struct Tick<R> {
    /// A report is due — encode and send it if there is a target.
    pub report: Option<R>,
}

/// What an inbound delivery turned into.
pub enum Inbound {
    /// Params accepted and applied. `blob` is what to persist; `source`
    /// is the sender's delivery hash — now also the report target, so
    /// the node that tuned us is the node that hears back.
    /// `verification` is what the signature check could say: it is
    /// `Unverified` unless the wrapper passed a signer identity (an
    /// allow-listed remote) via `signer`.
    Applied {
        blob: Vec<u8>,
        source: [u8; 16],
        verification: Verification,
    },
    /// An LXMF message, but not a valid params frame (or refused values).
    Rejected,
    /// Not an LXMF message.
    Ignored,
}

pub struct App<D: Device> {
    device: D,
    /// Where reports go — set by the last accepted params message, or
    /// by the wrapper explicitly.
    target: Option<[u8; 16]>,
    next_sample_ms: u64,
    samples_since_report: u32,
}

impl<D: Device> App<D> {
    /// Construct from a device at its compiled-in defaults, then
    /// overlay whatever the store holds.
    pub fn boot<S: ConfigStore, L: Log>(
        mut device: D,
        store: &mut S,
        now_ms: u64,
        log: &mut L,
    ) -> Self {
        let mut target = None;
        match store.load().and_then(|b| Self::split_blob(&b)) {
            Some((params, t)) => {
                device.apply(params, now_ms);
                target = t;
                log.line(
                    "[AUTO] ",
                    format_args!("config=loaded target={}", t.is_some()),
                );
            }
            None => {
                device.apply(D::Params::DEFAULT, now_ms);
                log.line("[AUTO] ", format_args!("config=default"));
            }
        }
        Self {
            device,
            target,
            next_sample_ms: 0,
            samples_since_report: 0,
        }
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    pub fn target(&self) -> Option<[u8; 16]> {
        self.target
    }

    /// Set the report target by hand (a debug-port command, a
    /// provisioning step). Returns the blob to persist.
    pub fn set_target(&mut self, target: [u8; 16]) -> Vec<u8> {
        self.target = Some(target);
        self.blob()
    }

    /// One pass of the main loop. Cheap when nothing is due.
    ///
    /// `io` samples the input and applies the output — a
    /// `(&mut sensor, &mut actuator)` tuple for a regulator, or a
    /// bank adaptor ([`crate::iotest::IoBus`]) for a panel.
    pub fn tick<I: Io<D>, C: Clock, L: Log>(
        &mut self,
        io: &mut I,
        clock: &C,
        log: &mut L,
    ) -> Tick<D::Report> {
        let now = clock.now_ms();
        if now < self.next_sample_ms {
            if let Some(out) = self.device.hold(now) {
                io.drive(&out);
            }
            return Tick { report: None };
        }
        self.next_sample_ms = now + self.device.params().sample_ms() as u64;

        let input = io.sample();
        if !D::input_valid(&input) {
            log.line("[AUTO] ", format_args!("input=invalid failsafe"));
        }
        let out = self.device.step(input, now);
        io.drive(&out);

        self.samples_since_report += 1;
        let every = self.device.params().report_every().max(1);
        if self.samples_since_report >= every {
            self.samples_since_report = 0;
            return Tick {
                report: Some(self.device.report(now)),
            };
        }
        Tick { report: None }
    }

    /// Handle an opportunistic LXMF delivery addressed to `our_dest`.
    ///
    /// On `Applied` the caller owns persistence — save `blob` through
    /// whatever store the board has (directly, or through the flash
    /// task on a SoftDevice build). `signer`, when the wrapper keeps
    /// the remote's identity (allow-list), verifies the sender's
    /// signature; `None` leaves `verification` as `Unverified`.
    pub fn on_message<L: Log>(
        &mut self,
        on_air: &[u8],
        our_dest: &[u8; 16],
        now_ms: u64,
        log: &mut L,
        signer: Option<&Identity>,
    ) -> Inbound {
        match lxmf::decode_params::<D::Params>(on_air, our_dest, signer) {
            lxmf::Decoded::Params {
                params,
                source,
                verification,
            } => {
                self.device.apply(params, now_ms);
                self.target = Some(source);
                self.next_sample_ms = 0;
                log.line(
                    "[AUTO] ",
                    format_args!(
                        "params=applied from={:02x}{:02x}{:02x}{:02x} verified={}",
                        source[0],
                        source[1],
                        source[2],
                        source[3],
                        verification == Verification::Valid
                    ),
                );
                Inbound::Applied {
                    blob: self.blob(),
                    source,
                    verification,
                }
            }
            lxmf::Decoded::NotParams => Inbound::Rejected,
            lxmf::Decoded::BadFrame => Inbound::Ignored,
        }
    }

    /// The signed, opportunistic on-air bytes of a report — `None` when
    /// there is no target yet.
    pub fn report_message(
        &self,
        identity: &Identity,
        our_dest: &[u8; 16],
        report: &D::Report,
        now_ms: u64,
    ) -> Option<Vec<u8>> {
        let target = self.target?;
        lxmf::encode_report(identity, our_dest, &target, report, now_ms as f64 / 1000.0)
    }

    /// The persisted shape.
    pub fn blob(&self) -> Vec<u8> {
        let mut b = self.device.params().encode();
        match self.target {
            Some(t) => {
                b.extend_from_slice(&t);
                b.push(TARGET_PRESENT);
            }
            None => {
                b.extend_from_slice(&[0u8; 16]);
                b.push(0);
            }
        }
        b
    }

    fn split_blob(b: &[u8]) -> Option<(D::Params, Option<[u8; 16]>)> {
        if b.len() < 17 {
            return None;
        }
        let (frame, tail) = b.split_at(b.len() - 17);
        let params = D::Params::decode(frame).ok()?;
        let target = (tail[16] == TARGET_PRESENT).then(|| tail[..16].try_into().unwrap());
        Some((params, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hal::NoLog;
    use crate::thermostat::{Thermostat, ThermostatParams};

    struct FakeStore(Option<Vec<u8>>);
    impl ConfigStore for FakeStore {
        fn load(&mut self) -> Option<Vec<u8>> {
            self.0.clone()
        }
        fn save(&mut self, blob: &[u8]) -> bool {
            self.0 = Some(blob.to_vec());
            true
        }
    }
    struct FakeSensor(Option<f32>);
    impl Sensor for FakeSensor {
        fn read(&mut self) -> Option<f32> {
            self.0
        }
    }
    struct FakeActuator(bool);
    impl Actuator for FakeActuator {
        fn set(&mut self, on: bool) {
            self.0 = on;
        }
    }
    struct FakeClock(u64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn boot_from_empty_store_uses_defaults() {
        let mut store = FakeStore(None);
        let app = App::boot(Thermostat::new(), &mut store, 0, &mut NoLog);
        assert_eq!(*app.device().params(), ThermostatParams::DEFAULT);
        assert!(app.target().is_none());
    }

    #[test]
    fn blob_roundtrips_params_and_target() {
        let mut store = FakeStore(None);
        let mut app = App::boot(Thermostat::new(), &mut store, 0, &mut NoLog);
        let blob = app.set_target([3u8; 16]);
        store.save(&blob);
        let again = App::boot(Thermostat::new(), &mut store, 0, &mut NoLog);
        assert_eq!(again.target(), Some([3u8; 16]));
    }

    #[test]
    fn corrupt_blob_falls_back() {
        let mut store = FakeStore(Some(alloc::vec![0xAA; 50]));
        let app = App::boot(Thermostat::new(), &mut store, 0, &mut NoLog);
        assert_eq!(*app.device().params(), ThermostatParams::DEFAULT);
    }

    #[test]
    fn tick_paces_on_sample_ms_and_fails_safe() {
        let mut store = FakeStore(None);
        let mut app = App::boot(Thermostat::new(), &mut store, 0, &mut NoLog);
        let mut sensor = FakeSensor(Some(10.0)); // well below the 21 °C default
        let mut act = FakeActuator(false);
        let t = app.tick(
            &mut (&mut sensor, &mut act),
            &FakeClock(0),
            &mut NoLog,
        );
        assert!(act.0, "cold → heater on");
        assert!(
            t.report.is_none(),
            "report_every=6: not on the first sample"
        );
        // Before the next sample is due: nothing changes.
        sensor.0 = None;
        app.tick(
            &mut (&mut sensor, &mut act),
            &FakeClock(10),
            &mut NoLog,
        );
        assert!(act.0);
        // Next sample with no reading → off.
        let period = ThermostatParams::DEFAULT.sample_ms as u64;
        app.tick(
            &mut (&mut sensor, &mut act),
            &FakeClock(period),
            &mut NoLog,
        );
        assert!(!act.0, "no reading → failsafe off");
    }
}
