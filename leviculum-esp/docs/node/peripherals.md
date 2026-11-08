# `node::peripherals` — the I2C add-on sensors

`src/node/peripherals.rs`

BMV080 (particulate) and BME690 (environmental) share one I2C0 pair —
the Qwiic pads on the XIAO kit. They are add-ons, so presence is a
runtime question: both are probed once at boot and the answer is
printed (`state=up` / `state=absent` / `state=present
note=probe-only-no-sdk`), never assumed.

## Bus sharing

Both probes run *before* the bus is handed out — `SharedI2c::new` is
the moment `i2c` stops being borrowable by name. The bus is then boxed
inside `Sensors` (stable address: the drivers hold a raw pointer to it
and `Sensors` moves) and handed to each driver as a `SharedI2c` — one
handle per driver, exclusivity by sequencing, not the type system.

## Features

| feature | driver | cadence |
|---|---|---|
| `bmv080-sdk` | Bosch's licensed blob | duty cycling: 10 s integration every 60 s; `serve` drains the event FIFO each loop pass, logs a `pm1/pm2_5/pm10` line when a new measurement lands |
| `bme690` | Bosch's BSD-3 driver, compiled | forced mode, one `t/p/h/gas` sample per 5 s banner tick — the 100 ms heater soaks in easily |

An I2C config failure is fatal (`halt_blinking`); a missing sensor is
not. Without either feature the probe lines are the whole story for
that sensor.
