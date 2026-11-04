# Thermostat — on/off regulator with hysteresis

The smallest device that exercises every part of the contract: params
with a validity rule, a report with a failsafe flag, and state that
`apply` must reset.

```text
DS18B20 ──▶ Sensor ──▶ App<Thermostat>::tick ──▶ Actuator ──▶ heater relay
                          │
   THRS report ◀── report()      step() ──▶ heater on/off decision
   THRM params ───▶ apply()
```

## Behaviour

- Temperature below `setpoint_c − hysteresis_c` → heater **on**
- Above `setpoint_c + hysteresis_c` → heater **off**
- Inside the band → hold previous state
- No reading (`Sensor::read → None`) → **off** (failsafe) and
  `sensor_lost` set in the report
- `apply()` clears the output — a new setpoint re-decides from scratch
  rather than letting an old band hold the heater

## Wire frames

### `THRM` v1 — params, 14-byte payload

| Offset | Type | Field | Rule (`valid()`) |
|---|---|---|---|
| 0 | f32 LE | `setpoint_c` | finite, −50..=150 °C |
| 4 | f32 LE | `hysteresis_c` | finite, ≥ 0 |
| 8 | u32 LE | `sample_ms` | ≥ 500 |
| 12 | u16 LE | `report_every` | ≥ 1 |

Defaults: `setpoint=21.0`, `hysteresis=0.5`, `sample_ms=10 000`,
`report_every=6` (one report per minute at the default cadence).

### `THRS` v2 — report, 13-byte payload

| Offset | Type | Field |
|---|---|---|
| 0 | f32 LE | `temperature_c` (NaN when sensor lost) |
| 4 | f32 LE | `setpoint_c` |
| 8 | u8 | flags: bit0 `on`, bit1 `sensor_lost` |
| 9 | u32 LE | `uptime_s` |

> **v1 → v2**: uptime grew from 3 B to 4 B — decode with the same crate
> version on both sides, or pin the version byte.

## Board wiring (XIAO ESP32-S3 kit, `xiao_s3_auto.rs`)

| Role | Pin | HAL adaptor |
|---|---|---|
| DS18B20 data | GPIO2 (D0), open-drain + pull-up | `Ds18b20Sensor` |
| Relay/LED | GPIO4 (D2), push-pull, active-high | `GpioActuator` |
| Config sector | flash @ `0x0070_2000` | `FlashStore` |
| Clock | `uptime_ms()` | `UptimeClock` |
| Log | USB serial | `UsbLog` |

nRF52840 wrapper: `leviculum-nrf/src/automation.rs` — `GpioActuator`,
`NvmcStore` (NVMC page), `EmbassyClock`, `FirmwareLog`.

## Loop sketch

```rust
let mut app = App::boot(Thermostat::new(), &mut cfg_store, now, &mut log);
loop {
    // mesh poll → let signer = lxmf::peek_source(data)
    //                 .and_then(|src| node.storage().get_identity(&src));
    //             app.on_message(data, &our_dest, now, &mut log, signer)
    //   on Inbound::Applied { blob, .. } → cfg_store.save(&blob)

    let tick = app.tick(&mut (&mut sensor, &mut actuator), &clock, &mut log);
    if let Some(report) = tick.report {
        if let Some(on_air) = app.report_message(&identity, &our_dest, &report, now) {
            mesh.send_single_packet(&target_hash, &on_air);
        }
    }
}
```

## Host side — `thermostat_host`

```sh
cargo run -p lnmsg --example thermostat_host -- \
    --to <board-lxmf-hash> \
    --setpoint 21.5 --hysteresis 0.3 --sample-ms 10000 --report-every 3 \
    --instance default
```

- `--to` is the board's `lxmf.delivery` hash — `[AUTO] dest=…` on the
  board's debug port.
- The host's own address becomes the board's report target (its identity
  persists under `LNMSG_HOME`, so reboots keep the pairing).
- Reports print as
  `[THRS] t=20.75 sp=21.5 on=true lost=false uptime=312s from=00b5…`.
- `--bench-hex` prints `AUTO LXMF <hex>` to paste into the board's debug
  port — exercises `on_message` without a mesh.
- `--listen-only` skips the params send (just print reports).

## Debug-port commands (xiao_s3_auto)

| Command | Effect |
|---|---|
| `AUTO TARGET <32hex>` | set the report destination by hand |
| `AUTO GET` | print running params |
| `AUTO LXMF <hex>` | feed an opportunistic params message over serial |

## Failure modes

| Symptom | Cause | Behaviour |
|---|---|---|
| `params=rejected` in log | invalid values or bad frame | params unchanged |
| `not-lxmf` | junk bytes on the destination | ignored |
| `sensor_lost=true` in THRS | DS18B20 absent/CRC fail | heater forced off |
| `config=default` at boot | empty/corrupt flash sector | defaults + no target |
