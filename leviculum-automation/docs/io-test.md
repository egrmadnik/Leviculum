# io-test — multi-channel I/O exerciser

The device that exercises the *multi-channel* HAL traits —
`DigitalIn`, `DigitalOut`, `AnalogIn` — the way the thermostat
exercises `Sensor`/`Actuator`. It exists to verify board wiring,
the `Io` glue and the params/report path on a panel of relays,
switches and ADC channels, before a real device goes on those pins.

```text
switches ─▶ DigitalIn ─┐
pots     ─▶ AnalogIn  ─┤ IoBus ─▶ App<IoTest>::tick ─▶ DigitalOut ─▶ relays/LEDs
                       └             │                       ▲
                IOTS report ◀── report()           step() ──┘
                IOTC params ───▶ apply()
```

## Operating modes

| Mode | `flags` | Behaviour |
|---|---|---|
| **Mirror** (default) | `FLAG_MIRROR` (0x01) | `dout = din` every cycle — wire DI*n* to a switch, watch DO*n* and the report follow it |
| **Remote** | flags = 0 | outputs take `dout_mask` from the remote; inputs still sampled and reported |

## Channels

| Bank | Channels | Notes |
|---|---|---|
| Digital in | 8 (`DI_CHANNELS`) | bitmask, bit *n* = DI*n* |
| Digital out | 8 (`DO_CHANNELS`) | bitmask, bit *n* = DO*n* |
| Analog in | 4 (`AI_CHANNELS`) | raw ADC counts (12-bit on the kits) |

Channels the board does not populate return `None` from `read` and show
as `u16::MAX` (analog) or an absent `din` (digital) in the report.

## Wire frames

### `IOTC` v1 — params, 8-byte payload

| Offset | Type | Field | Rule (`valid()`) |
|---|---|---|---|
| 0 | u8 | `flags` | only `FLAG_MIRROR` defined; other bits must be 0 |
| 1 | u8 | `dout_mask` | applied when `FLAG_MIRROR` clear |
| 2 | u32 LE | `sample_ms` | ≥ 20 |
| 6 | u16 LE | `report_every` | ≥ 1 |

Defaults: mirror on, `dout_mask=0`, `sample_ms=500`, `report_every=20`
(one report per 10 s).

### `IOTS` v1 — report, 15-byte payload

| Offset | Type | Field |
|---|---|---|
| 0 | u8 | `din` — last DI bitmask |
| 1 | u8 | `dout` — mask currently driven |
| 2 | u8 | flags: bit0 mirror mode, bit1 `DIN_VALID` |
| 3 | 4× u16 LE | `adc[0..3]` raw counts; `0xFFFF` = no reading |
| 11 | u32 LE | `uptime_s` |

`DIN_VALID` (0x02) clears when *every* DI channel fails to read — a
dead input rail shows up as `din=0x00` with the flag off, not as a
plausible-looking zero byte.

## `IoBus` — the wrapper glue

`IoBus` bundles the three bank traits into the single `Io<IoTest>`
impl `App::tick` needs:

```rust
let mut io = IoBus {
    din: &mut di_bank,   // impl DigitalIn
    dout: &mut do_bank,  // impl DigitalOut
    adc: &mut adc_bank,  // impl AnalogIn
};
let tick = app.tick(&mut io, &clock, &mut log);
```

`sample()` reads all 8 DI channels into a bitmask (`None` if every
channel failed) and all 4 ADC channels; `drive()` writes the 8-bit
output mask to every DO channel.

A board wrapper implements each trait once — a `DigitalIn` over a
GPIO bank or an I²C expander, `DigitalOut` over relays, `AnalogIn`
over `saadc` channels — the same shape `GpioActuator`/`NvmcStore`
already fill for the thermostat.

## Host simulation — `examples/io_test.rs`

No hardware needed; exercises the full path including a signed LXMF
params message and a persist/reboot cycle:

```sh
cargo run -p leviculum-automation --example io_test
```

```text
[AUTO] config=default
[sim] board dest=c3193f…

== mirror mode (dout follows din) ==
[sim] t0 din=00000001 -> dout=00000001
  [IOTS] din=00000001 dout=00000001 flags=0x03 adc=[ 959, 1936, 2913, ----] uptime=1s
[sim] t1 din=00000110 -> dout=00000110
[sim] t2 din=10100101 -> dout=10100101

== remote params via LXMF (mirror off, mask=0xF0) ==
[AUTO] params=applied from=b43f0a27 verified=true
[sim] remote mode: din=00001111 dout=11110000 (mask wins)

== dead input rail ==
  [IOTS] din=00000000 dout=11110000 flags=0x00 adc=[ 699, 1676, 2653, ----] uptime=5s

== reboot with persisted config ==
[AUTO] config=loaded target=true
```

The `mod fake` block in that file is the whole wrapper surface: three
bank impls, a `RamStore`, a hand-advanced clock — on real hardware
each becomes a few lines over embassy/esp-hal peripherals.

## Bench checklist on a real board

1. **DO check** — remote mode, walk `dout_mask` through `01,02,04,…,80`:
   each relay clicks in order.
2. **DI check** — mirror mode, close each switch: DO and `IOTS.din`
   follow per-channel.
3. **AI check** — sweep each pot: `IOTS.adc[n]` sweeps 0→4095; a dead
   channel stays `----` (0xFFFF).
4. **Failsafe** — cut the DI rail: `DIN_VALID` clears, `din=0x00`,
   mirror drops outputs to 0.
5. **Persist** — apply params, power-cycle: `[AUTO] config=loaded`,
   mode/mask and report target survive.

## Failure modes

| Symptom | Meaning |
|---|---|
| `din=0x00` + `DIN_VALID` clear | every DI read failed — wiring, rail, or pull-ups |
| `adc[n] = 0xFFFF` | that channel unpopulated or conversion failed |
| `params=rejected` | bad flags (reserved bits set), `sample_ms < 20`, or frame corruption |
| outputs pulse low on re-config | *won't happen by design*: `apply()` holds `dout` until the next `step` re-derives it |
