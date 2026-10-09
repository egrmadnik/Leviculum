# Automation devices on an LNode

An LNode can be more than a relay: a thermostat, a pump controller, a
counter — a _device_ with parameters a remote node sets over LXMF,
kept in flash, reporting its status back the same way. The
`leviculum-automation` crate is the template: the device logic and the
config/report/LXMF plumbing in one portable `no_std` crate, and a
per-board wrapper of about a hundred lines that names pins and wires
in the radio.

```text
  remote node ──LXMF(content = Params frame)──▶┌────────────────┐
                                                │  App<Device>   │──▶ flash
  remote node ◀──LXMF(content = Report frame)──│                │
                                                └─────┬────▲───┘
                                          Actuator  ◀┘    └ Sensor
```

## Layers

| Layer                                                             | Where                                                                                                         | Changes when         |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- | -------------------- |
| HAL traits — `Sensor`, `Actuator`, `ConfigStore`, `Clock`, `Log`  | `leviculum-automation/src/hal.rs`                                                                             | never (the contract) |
| Wire envelope — magic, version, CRC32                             | `src/frame.rs`                                                                                                | never                |
| Device contract — `Params`, `Report`, `Device`                    | `src/device.rs`                                                                                               | never                |
| App loop — boot, cadence, actuate, persist, inbound/outbound LXMF | `src/app.rs`, `src/lxmf.rs`                                                                                   | never                |
| **The device** — e.g. `Thermostat`                                | `src/thermostat.rs`                                                                                           | per device           |
| **The wrapper** — pins, flash sector, radio, NodeCore             | `leviculum-esp/src/bin/xiao_s3_auto.rs`, `leviculum-esp/src/automation.rs`, `leviculum-nrf/src/automation.rs` | per board            |

## Writing a device

Implement three traits in one file:

```rust
impl Params for MyParams {
    const MAGIC: [u8; 4] = *b"MYPR";  // the frame type on the wire
    const VERSION: u8 = 1;
    const DEFAULT: Self = ...;        // what runs on empty flash
    fn write(&self, out: &mut Vec<u8>) { ... }   // LE fields
    fn read(c: &mut Cursor) -> Result<Self, FrameError> { ... }
    fn valid(&self) -> bool { ... }   // refuse NaN, zero periods …
    fn sample_ms(&self) -> u32 { ... }
}
impl Report for MyReport { const MAGIC = *b"MYRP"; fn write(..) }
impl Device for MyDevice {
    type Params = MyParams; type Report = MyReport;
    fn apply(&mut self, p, now)  // new tune → reset accumulated state
    fn step(&mut self, input: Option<f32>, now) -> Output  // None → failsafe
    fn report(&self, now) -> MyReport
}
```

`Params::decode` is the envelope check + `read` + `valid` and refuses
as a unit — a bad frame never half-applies. The frame envelope and its
three rejections (magic, version, CRC) are tested once in `frame.rs`
and inherited by every device.

The worked example, `Thermostat`, is an on/off regulator with
hysteresis: heater on below `setpoint − hysteresis`, off above
`setpoint + hysteresis`, off on a missing reading. Its `THRM` params
frame is 14 bytes; its `THRS` report is 12.

## Writing a wrapper

The ESP32-S3 wrapper (`xiao_s3_auto`) is the reference. Its four
parts:

1. **Pins → HAL**: `Ds18b20Sensor(ds18b20 on D1)`, `GpioActuator(D3)`,
   `FlashStore { sector 0x702000 }`, `UptimeClock`. The nRF adaptors
   are `GpioActuator`, `NvmcStore`, `EmbassyClock`, `FirmwareLog` in
   `leviculum-nrf/src/automation.rs`.
2. **Identity + mesh**: the node's identity from flash (or generated
   once), SX1262 up, a boxed `NodeCore`, `lxmf.delivery` registered and
   announced every 60 s.
3. **The loop**: `app.tick(sensor, actuator, clock, log)` → actuate and
   possibly a report → `app.report_message(..)` → `send_single_packet`.
   Radio RX → `handle_packet` → `PacketReceived` for our destination →
   `app.on_message(..)`, which applies, persists, and remembers the
   sender as the report target.
4. **Bench path**: `AUTO GET`, `AUTO TARGET <32hex>`, `AUTO LXMF <hex>`
   on the debug port — the last feeds a params message in exactly as
   the radio would deliver it; `thermostat_host --bench-hex` produces
   the line.

A wrapper for another device on the same board changes line 1 and the
`Thermostat::new()`. A wrapper for the same device on another board
changes the adaptors and the radio glue; the loop body is the same
calls.

## Flash

The app persists one blob — `Params` frame + 16-byte report target +
presence flag — through `ConfigStore`. Both stores write
`[len u16][blob][crc32]` into one 4 KiB sector/page, so the sector reads
as "defaults, no target" on fresh or torn flash. On the ESP32-S3 the
sector is `0x702000` (above the PID node's two). On the nRF the
`NvmcStore` is only legal while the SoftDevice is not running; a BLE
build routes `Inbound::Applied { blob, .. }` to the shared-flash task
instead — see the module docs.

## What the LXMF path does and does not check

Inbound params arrive as the _opportunistic_ LXMF representation —
`NodeCore` already decrypted them to our registered destination, which
is the destination check. The frame CRC refuses corruption. What is
**not** checked is the sender's signature: a board has no copy of the
remote's identity until an allow-list or a known-destinations store
supplies one, and the crate says so rather than pretending. Reports out
are fully signed with the node identity.

## The host side

`lnmsg/examples/thermostat_host.rs` is the remote node: it attaches to
the running `lnsd` with `lnmsg`'s engine, resolves the board, sends a
`THRM` frame as the body of a direct LXMF message, and prints every
`THRS` report that comes back. `--bench-hex` prints the same message as
a debug-port line instead of sending it. Step-by-step verification,
from host tests to a live mesh: [Testing an automation
device](automation-testing.md).

## Status

- `leviculum-automation`: 12 host tests — frame rejections, thermostat
  band, app boot/persist/failsafe, LXMF params and report round-trips.
- `xiao_s3_auto` builds on the Xtensa toolchain; `leviculum-nrf`
  clippy-clean with the adaptors.
- Hardware: not yet exercised. The transport underneath (`radio.rs` +
  `NodeCore`) is the one `xiao_s3` uses.
