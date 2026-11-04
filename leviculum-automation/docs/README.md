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
| LXMF carrier — params out of a message (`decode_params`, `peek_source`), report into one (`encode_report`) | `lxmf` | this crate |
| Worked devices — thermostat, I/O exerciser, echo loopback | `thermostat`, `iotest`, `echo` | examples |

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

- **`Params`** has a 4-byte frame magic, a version, an optional
  `PAYLOAD_LEN` capacity hint (for single-allocation frame encoding via
  `frame::encode_with`), `DEFAULT`, a wire layout (`write`/`read` via
  `Buf`/`Cursor`), a `valid()` rule, and the cadence (`sample_ms`,
  `report_every`). `report_every() == 0` means on-demand reporting only
  (a single report immediately after new params are applied, with no
  periodic reports when idle). `Params::decode` refuses bad envelopes,
  wrong versions, CRC errors and invalid values — a remote can never
  half-apply a config.
- **`Report`** is the same wire shape (`MAGIC`, `VERSION`, `PAYLOAD_LEN`,
  `write`/`read`) with no validity rule.
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

Three impls ship in the crate:

- **`(&mut S, &mut A)`** — any `Sensor`+`Actuator` pair drives a
  single-channel regulator (`Input = Option<f32>`, `Output = Output`):
  ```rust
  app.tick(&mut (&mut sensor, &mut actuator), &clock, &mut log);
  ```
- **`iotest::IoBus { din, dout, adc }`** — bundles the three bank traits
  for `App<IoTest>`.
- **`NoIo`** — for devices with `Input = ()` and `Output = ()` (such as
  `Echo`).

## Remote control flow

1. A remote node (e.g. `lnmsg`'s `thermostat_host` example or
   `tests/echo-term.py`) resolves the board's `lxmf.delivery` hash and
   sends an opportunistic LXMF message (`DeliveryMethod::Opportunistic`)
   whose **content is a `Params` frame**.
2. The wrapper can inspect the sender hash up front with
   `lxmf::peek_source(on_air)` to look up a known `Identity` in
   `node.storage()`. `App::on_message(on_air, our_dest, now_ms, log, signer)`
   unpacks, verifies (when `signer` is supplied), CRC/validity-checks and
   applies the params, **sets the sender as the report target**, and arms
   an immediate report (`report_due = true`).
3. On `Inbound::Applied { blob, .. }` the *wrapper* persists `blob` —
   on a SoftDevice build flash writes must go through a task; the app
   deliberately does not touch the store itself.
4. The very next `App::tick` emits a confirmation `Report` immediately
   (and, when `params.report_every() > 0`, every `report_every`-th sample
   thereafter); the wrapper hands `app.report_message(...)` to the mesh.

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
| `Echo` — LXMF loopback, no I/O at all | `ECHC` | `ECHS` | `()` | `()` | [echo.md](echo.md) |

**Writing your own device** → [custom-device.md](custom-device.md) —
the checklist, the contract, and the pitfalls in one page.

**Message flow (Remote ↔ XIAO `auto_echo`)** → [message-flow.md](message-flow.md) —
layer-by-layer packet anatomy, sequence diagram, and annotated XIAO debug-port trace.

**Fast local stationary network setup** → [local-stationary-network.md](local-stationary-network.md) —
configuring `lnsd` (`/etc/reticulum/config`) and XIAO LoRa nodes for low-latency, isolated stationary automation networks.

## Host-side consumers

- `lnmsg/examples/thermostat_host.rs` — sets thermostat params and
  prints reports against a live `lnsd`/`rnsd`.
- `leviculum-automation/examples/io_test.rs` — the full io-test path on
  the host: `cargo run -p leviculum-automation --example io_test`.
- `leviculum-automation/examples/echo_node.rs` — host-side `App<Echo>`
  simulation: `cargo run -p leviculum-automation --example echo_node`.
- `leviculum-automation/tests/echo-term.py` — live end-to-end test
  script driving `lxmf-node` (`send_opportunistic`) against an
  `auto_echo` LNode over LoRa/serial (`lnsd`).

## Security note

Inbound signatures are verified whenever `on_message`'s `signer`
parameter is `Some(&Identity)`. Wrappers with a Reticulum identity store
 can pass
`lxmf::peek_source(data).and_then(|src| node.storage().get_identity(&src))`
(or an operator allow-list from the `mgmt` path). `Inbound::Applied.verification`
reports what the check could say — `Unverified` when `signer` is `None`,
`Valid` when verified. The frame CRC and `valid()` always gate the
payload.
