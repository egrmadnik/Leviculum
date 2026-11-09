# `node::display` — the LNode's status screen

`src/node/display.rs` (`display` feature)

The XIAO ESP32-S3 kit has no panel of its own, but its Qwiic add-on
pair (D4/D5) is the natural home for an I2C OLED. This module is the
LNode's half of that: a [`Display`] built at boot over the *shared*
sensor bus, rendering the same `leviculum-screen` status model the nRF
boards and the V4 probe paint — locked by the same golden-frame host
tests.

## What the panel shows

The node's own story, at the banner cadence (5 s):

```
leviculum XIAO S3
id: <identity prefix>      ← leviculum_screen::ident_short
RX: 12345 TX: 42           ← carriers::RX_PACKETS / TX_PACKETS
Bat: -- (no feature)       ← the kit has no battery sense
GPS: none                  ← the painter's absence field
                           (heartbeat dot, top-right)
```

The painter's absence fields are honest: `BatteryStatus::FeatureOff`,
`GnssStatus::NoHardware`, no BLE peers, no propagation fill — the kit
carries none of them.

## The panel

An SSD1306-family I2C OLED on the Qwiic pair. Its SA0 strap puts it at
**0x3C or 0x3D**; the driver probes both at init (a NACK on the wrong
address is harmless) and keeps whichever answers — the boot log says
which: `[OLED] state=up addr=0x3c`.

## Why it lives in the main loop, not a task

The panel shares the sensor bus, so rendering happens in the main
loop's banner tick — the same thread the sensors poll on. Both are
synchronous, never-yielding calls, so they can never interleave: the
`SharedI2c` contract, extended to the display. The ~20 ms a full blit
takes once per 5 s is noise next to the loop's 100 ms pass.

## The seam

```
board.rs:  Wiring.display: Option<DisplayWiring>   (reset net only —
           the bus is opened by peripherals::start, the pins are the
           sensor pins)
node::run: Display::new(wiring, identity_hash, i2c_bus, log)
           → None when no wiring, or the panel refused to come up
           ([OLED] state=down reason=init) — the node runs on without
mainloop:  banner tick → display.render(log) — paint + blit, failures
           logged ([OLED] state=err reason=write), never fatal
```

The driver itself (`src/display.rs`, the SSD1315) is shared with the
V4 probe, which owns its I2C bus outright instead of sharing.

## Key points

- **A panel is an add-on, discovered at boot.** Feature on, panel
  absent → `state=down`, the node runs on. Same no-hardware
  discipline as the sensors.
- **The bus is shared, safely by construction** — one thread, blocking
  calls, no yields, no interleave.
- **The frames are the shared ones** — `leviculum-screen`, golden-frame
  tested; the ESP side draws nothing of its own.