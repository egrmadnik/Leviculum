# Telemetry — BMV080 and BME690

What the two add-on sensors on `xiao_s3` (Seeed XIAO ESP32-S3 +
Wio-SX1262, Qwiic pair on D4/D5) report, and how their readings map onto
the [telemetry format](telemetry.md) — one `FIELD_TELEMETRY` msgpack map
per report, sensor ID to packed value. The rules that page sets apply
here unchanged: a sensor that is fitted but produced no reading encodes
as its SID → nil, and a sensor not fitted contributes no key at all.

## BME690 — environment

Four quantities per measurement, all from one forced-mode sample
(×16 oversampling, gas heater at 300 °C for 100 ms — the values the
SensorAPI's own `forced_mode` example ships):

| Quantity | Unit | SID | Notes |
|---|---|---|---|
| Temperature | °C | `0x07` (`SID_TEMPERATURE`) | Existing Sideband sensor — packs as a bare float, displays in Sideband and Columba with no work on their side |
| Pressure | Pa | `0x03` (`SID_PRESSURE`) | Existing Sideband sensor ID; `leviculum-lxmf` does not encode it yet — the addition is one codec arm following `sense.py`'s packed form |
| Relative humidity | % | `0x06` (`SID_HUMIDITY`) | Same: existing SID, codec arm to add |
| Gas resistance | Ω | custom | **Not a Sideband sensor.** The resistive response of the 300 °C hotplate — a proxy for reducing/oxidising gases, not an IAQ or VOC index. Any IAQ figure requires BSEC, a separate proprietary Bosch library that is not integrated |

The first three climb no rung of the extension ladder — rung one,
existing SIDs, is where they sit. Gas resistance needs a custom ID and a
consumer that knows it.

## BMV080 — particulate matter

Three concentrations plus two health flags per duty-cycle window (10 s
integration, 60 s period — the SDK's own battery-oriented defaults):

| Quantity | Unit | SID | Notes |
|---|---|---|---|
| PM1 | µg/m³ | custom | Particles ≤ 1 µm |
| PM2.5 | µg/m³ | custom | Particles ≤ 2.5 µm — the headline value a viewer would chart |
| PM10 | µg/m³ | custom | Particles ≤ 10 µm |
| Obstructed | bool | custom | The sensor sees its optical path blocked — the values ship, flagged |
| Out of range | bool | custom | Concentration exceeded the measurement range — clamped at the bound, flagged |

Neither Sideband nor Columba defines a particulate sensor today, so all
five are custom IDs: they travel fine — the codec skips unknown SIDs on
read, exactly as `Telemeter.from_packed` does — but display requires a
consumer that knows them. `lnomad`/`lndecode` first; Sideband later if
it adopts the IDs.

## Cadence and size

- **BMV080**: one report per duty cycle — 60 s by default, because the
  sensor is current-hungry (laser plus fan). The cadence *is* the power
  budget; cadence stays policy in the reporting node, never a number
  inside the codec.
- **BME690**: one forced sample per report — the heater pulse dominates
  (~100 ms at ~30 mW). A 5 s cadence on the bench; minutes on a live
  node.
- Both omit on failure: a report with no PM key is a board that could
  not integrate that window, not a reading of zero.

## Semantic limits

- `gas_ohm` is a raw heater resistance, not air quality — the field is
  `gas_resistance`, and nothing in the report calls it `aqi`. That is a
  claim the field name makes, and it is kept honest deliberately.
- The BMV080 values are already the post-processed concentrations the
  Bosch SDK computes inside `lib_bmv080.a`; no further calibration lives
  in the firmware.
- Custom SIDs are stable once assigned. Choose values above `0x18` —
  past Sideband's defined range — so they cannot collide with a future
  `sense.py` addition.
