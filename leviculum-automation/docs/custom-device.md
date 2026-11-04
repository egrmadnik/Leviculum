# Writing a custom automation device

How to put a new device — a pump scheduler, a counter, an irrigation
controller, anything with config + inputs + outputs + status — on an
LNode. The rule: **you write one file in this crate; `App`, the LXMF
path, the flash path and the cadence come for free.**

## The contract, in one page

```rust
impl Device for MyDevice {
    type Params = MyParams;   // remote-set config (a frame)
    type Report = MyReport;   // periodic status (a frame)
    type Input  = /* one sample */;
    type Output = /* one actuation command */;

    fn params(&self) -> &Self::Params;
    fn apply(&mut self, params: Self::Params, now_ms: u64);
    fn step(&mut self, input: Self::Input, now_ms: u64) -> Self::Output;
    fn report(&self, now_ms: u64) -> Self::Report;
    // optional: fn input_valid(...) -> bool
    // optional: fn hold(&mut self, now_ms) -> Option<Self::Output>
}
```

## Checklist

### 1. Pick the shape of `Input` and `Output`

| Your device | `Input` | `Output` | `Io` impl |
|---|---|---|---|
| One sensor, one on/off output (thermostat, heater) | `Option<f32>` | `Output` | `(&mut sensor, &mut actuator)` — built in |
| Banks of pins/ADCs (I/O panel) | a snapshot struct (`IoSample`) | a mask struct (`IoCommand`) | `IoBus` or your own |
| Nothing to drive (echo, counter, beacon) | `()` | `()` | `NoIo` |
| Something else (PWM, stepper…) | whatever samples it | whatever commands it | write one |

`Input: Copy` — samples cross the trait by value.

### 2. Design the two frames

Two magics: one for params (what the remote *sets*), one for reports
(what the device *says*). Conventions in this crate:

```text
THRM (params) ↔ THRS (report)      IOTC ↔ IOTS      ECHC ↔ ECHS
             C = control/config                    S = status
```

Payload is little-endian fields in a fixed order, written with `Buf`,
read with `Cursor` — the same sequence both ways:

```rust
impl Params for PumpParams {
    const MAGIC: [u8; 4] = *b"PUMP";
    const VERSION: u8 = 1;
    const PAYLOAD_LEN: usize = 9; // optional hint for single-allocation encode()
    const DEFAULT: Self = Self { /* what runs when flash is empty */ };

    fn write(&self, out: &mut Buf<'_>) {
        out.f32(self.duty_on_s);
        out.u32(self.period_s);
        out.u8(self.enabled as u8);
    }
    fn read(c: &mut Cursor<'_>) -> Result<Self, FrameError> {
        Ok(Self { duty_on_s: c.f32()?, period_s: c.u32()?, enabled: c.u8()? != 0 })
    }
    /* … */
}
```

**Bump `VERSION` whenever the payload layout changes.** A v1 remote
writing a v2 board gets `FrameError::Version`, not silent misparse.

### 3. Write `valid()` — the remote's safety boundary

`Params::decode` refuses anything `valid()` rejects. This is the only
line between a corrupted/malicious frame and a pump running dry:

```rust
fn valid(&self) -> bool {
    self.enabled as u8 <= 1
        && self.duty_on_s.is_finite() && self.duty_on_s >= 0.0
        && self.duty_on_s <= self.period_s as f32
        && self.period_s >= 1 && self.period_s <= 86400
        && self.flags & !KNOWN_FLAGS == 0   // reject bits v2 hasn't defined yet
}
```

Also define `sample_ms()` (how often `step` runs) and `report_every()`
(report per N samples, or `0` for on-demand reporting only when new
params are applied) on `Params` — remote-tunable cadence, not constants,
unless the device truly has nothing to tune (see `Echo`). Note that
`App::on_message` always forces one immediate report on the next `tick`
after applying new params, even when `report_every() > 1`.

### 4. `apply()` — reset accumulated state

`apply` runs on boot (defaults or stored blob) **and** on every accepted
remote params message. Whatever the old tune accumulated — integrators,
phase timers, hysteresis state — must reset or be explicitly carried:

```rust
fn apply(&mut self, params: Self::Params, _now_ms: u64) {
    self.params = params;
    self.phase_started = None;   // re-time from scratch
    // outputs: hold or drop? Thermostat drops (new band must re-decide);
    // IoTest holds (a 0-pulse on every re-config is worse).
}
```

### 5. `step()` — one cycle, fail safe on bad input

```rust
fn step(&mut self, input: Self::Input, now_ms: u64) -> Self::Output {
    // no valid reading → choose the safe output, not the last one
}
```

`Device::input_valid(input)` tells the app to log a failsafe line; the
device still sees the value and decides. Override `hold(now_ms)` only if
the output decays or windows between samples (time-proportioned power).

### 6. `report()` — what the remote needs to see

Include the *decision-relevant* state, not internals: measured value,
applied setpoint, output state, any "something is wrong" flag
(`sensor_lost`, `DIN_VALID`), and `uptime_s` for staleness detection.

### 7. Tests — three that every device should have

```rust
#[test] fn params_frame_roundtrip_and_validity()   // encode/decode + rejects
#[test] fn report_frame_roundtrip()                // encode/decode
#[test] fn behaviour()                             // step() on scripted inputs
```

The `iotest.rs` tests show a fake-bank `Io` impl if you need one.

## The wrapper's job (unchanged by your device)

Whatever `D` is, the firmware loop stays:

```rust
let mut app = App::boot(MyDevice::new(), &mut store, now_ms, &mut log);
loop {
    // inbound: let signer = lxmf::peek_source(bytes)
    //              .and_then(|src| node.storage().get_identity(&src));
    //          app.on_message(bytes, &our_dest, now, &mut log, signer)
    //          → Inbound::Applied { blob, .. } → persist blob your way
    // every pass: app.tick(&mut io, &clock, &mut log)
    //          → tick.report → app.report_message(...) → mesh
}
```

Swap `Thermostat::new()` for your device and the `io` adaptor it needs —
that is the entire delta. See `xiao_s3_auto.rs` (ESP32-S3) and
`auto_echo.rs` (nRF52840) for the two reference wrappers.

## Wire budget

Keep frames small: an opportunistic LXMF packet on LoRa SF8/BW125
carries ~200 B of message, and every byte costs airtime. Params and
reports under ~50 B payload stay single-hop friendly; if you need more,
use a compact encoding (bitmasks, fixed-point integers, 3-byte time)
rather than growing the frame.

## Security

- `valid()` rejects malformed values; the frame CRC rejects corruption.
- Signatures are *not* verified unless the wrapper passes a `signer`
  identity to `on_message` — pin an operator allow-list where the
  device's actions matter (a heater is one thing, a gate actuator is
  another). `Inbound::Applied.verification` tells you what the check
  could say.
