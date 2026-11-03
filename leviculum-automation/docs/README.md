# leviculum-automation

A portable automation-device core for an LNode: config arrives over LXMF,
lives in flash, a device loop samples inputs and drives outputs on a
cadence, and status leaves as LXMF reports. Everything hardware is behind
traits, so the same `App<D>` runs on the nRF52840 kit, the ESP32-S3 kit,
and a host test with no board at all.

```text
remote node ──LXMF(content = Params frame)──▶ ┌──────────────┐
                                              │   App<D>     │ ◀── blob ─▶ ConfigStore (flash)
remote node ◀──LXMF(content = Report frame)── │  Device D    │
                                              └──────┬───────┘
                                                     │ step(input) → drive(output)
                                          Io<D> ───▶│◀─── hardware
```

## Layers

| Layer | Module | Owner |
|---|---|---|
| Hardware traits — `Sensor`, `Actuator`, `DigitalIn`, `DigitalOut`, `AnalogIn`, `ConfigStore`, `Clock`, `Log` | `hal` | the per-MCU wrapper |
| Wire envelope — 4 B magic, version byte, payload, CRC32 — plus `Cursor`/`Buf` field codecs | `frame` | this crate |
| The device contract — `Params` in, `Report` out, `step` in between | `device` | the device author |
| The app loop — boot-from-flash, sample/report cadence, `Io` sampling glue | `app` | this crate |
| LXMF carrier — params out of a message, report into one | `lxmf` | this crate |
| Worked devices — thermostat, I/O exerciser | `thermostat`, `iotest` | examples |

The crate is `no_std + alloc` and has no I/O of its own: it hands out byte
vectors and takes byte slices. The transport (NodeCore + radio) and every
peripheral live in the wrapper.

## The device contract

```rust
trait Device {
    type Params: Params;     // THRM, IOTC — what a remote may set
    type Report: Report;     // THRS, IOTS — what the device says
    type Input: Copy;        // Option<f32>, or an I/O bank snapshot
    type Output;             // Output{on}, or an output bitmask

    fn params(&self) -> &Self::Params;
    fn apply(&mut self, params: Self::Params, now_ms: u64);
    fn input_valid(input: &Self::Input) -> bool { true }
    fn step(&mut self, input: Self::Input, now_ms: u64) -> Self::Output;
    fn hold(&mut self, now_ms: u64) -> Option<Self::Output> { None }
    fn report(&self, now_ms: u64) -> Self::Report;
}
```

- **`Params`** has a 4-byte frame magic, a version, `DEFAULT`, a wire
  layout (`write`/`read` via `Buf`/`Cursor`), a `valid()` rule, and the
  cadence (`sample_ms`, `report_every`). `Params::decode` refuses bad
  envelopes, wrong versions, CRC errors and invalid values — a remote
  can never half-apply a config.
- **`Report`** is the same wire shape with no validity rule.
- **`input_valid`** marks a sample unusable (dead sensor rail). The app
  logs a failsafe line; `step` still sees the value and decides.
- **`hold`** runs between samples for outputs that decay or window on
  their own (PWM-style time proportioning); default keeps the last
  decision.

## Sampling and driving — `Io<D>`

The app knows nothing about the input/output *types*. The wrapper
supplies one `Io<D>` impl per board+device pair:

```rust
trait Io<D: Device> {
    fn sample(&mut self) -> D::Input;   // hardware → device
    fn drive(&mut self, out: &D::Output); // device → hardware
}
```

Two impls ship in the crate:

- **`(&mut S, &mut A)`** — any `Sensor`+`Actuator` pair drives a
  single-channel regulator (`Input = Option<f32>`, `Output = Output`):
  ```rust
  app.tick(&mut (&mut sensor, &mut actuator), &clock, &mut log);
  ```
- **`iotest::IoBus { din, dout, adc }`** — bundles the three bank traits
  for `App<IoTest>`.

## Remote control flow

1. A remote node (e.g. `lnmsg`'s `thermostat_host` example) resolves the
   board's `lxmf.delivery` hash and sends an opportunistic LXMF message
   whose **content is a `Params` frame**.
2. `App::on_message(on_air, our_dest, now_ms, log, signer)` unpacks,
   CRC/validity-checks and applies the params, **and sets the sender as
   the report target** — the node that tunes the device is the node that
   hears back.
3. On `Inbound::Applied { blob, .. }` the *wrapper* persists `blob` —
   on a SoftDevice build flash writes must go through a task; the app
   deliberately does not touch the store itself.
4. Every `params.report_every`-th sample produces a `Report`; the
   wrapper hands `app.report_message(...)` to the mesh.

## Wire formats

Every payload rides the same envelope:

```text
+--------+-----+------------------+--------+
| magic  | ver | payload (n B)    | crc32  |
| 4 B    | 1 B | little-endian    | 4 B LE |
+--------+-----+------------------+--------+
```

CRC-32 (IEEE `0xEDB88320`) covers magic+version+payload. A device
defines `MAGIC`/`VERSION` per frame type; bump `VERSION` when the
payload layout changes.

## Flash blob

```text
+-----------------------+---------------+------+
| Params frame (n B)    | target (16 B) | flag |
+-----------------------+---------------+------+
```

`flag = 0x54` when a report target is present. A corrupt sector decodes
as `None` → defaults + no target.

## Devices in this crate

| Device | Params | Report | Input | Output | Doc |
|---|---|---|---|---|---|
| `Thermostat` — on/off with hysteresis, failsafe on sensor loss | `THRM` | `THRS` | `Option<f32>` | `Output{on}` | [thermostat.md](thermostat.md) |
| `IoTest` — 8 DI, 8 DO, 4 AI; mirror or remote-mask modes | `IOTC` | `IOTS` | `IoSample` | `IoCommand{dout}` | [io-test.md](io-test.md) |

## Host-side consumers

- `lnmsg/examples/thermostat_host.rs` — sets thermostat params and
  prints reports against a live `lnsd`/`rnsd`.
- `leviculum-automation/examples/io_test.rs` — the full io-test path on
  the host: `cargo run -p leviculum-automation --example io_test`.

## Security note

Inbound signatures are *not* verified by default: a board typically has
no copy of the remote's identity. `on_message`'s `signer` parameter
verifies when the wrapper keeps one (an allow-list of operator
identities from the `mgmt` path). `Inbound::Applied.verification`
reports what the check could say — `Unverified` does not mean "bad",
it means "not checked". Wrappers that can pin operator identities
should; the CRC and `valid()` still gate everything else.
