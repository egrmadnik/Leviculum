# `node::boot` — log, LED, banner

`src/node/boot.rs`

Boot owns three things: the USB debug log (`UsbLog` over the Serial/JTAG
port — the same port the flasher reads back), the status LED, and the
repeating banner.

- `early::<B>(usb, led_pin) -> Booted` — open the log, print the
  `[BOARD]`/image identification, park the LED dark. The parked LED is
  the only boot proof on a unit with no cable attached.
- `blink(led, active_high)` — one 30 ms flash, honouring board polarity.
- `halt_blinking::<B>(boot) -> !` — for a peripheral that cannot be
  configured (e.g. the SPI bus won't open): blink and re-print the banner
  forever. A board that reports a broken bus is diagnosable; one that
  halts silently is not.

`BANNER_INTERVAL_MS` is 5 s — the same cadence as the nRF boards. A
reader who attaches after boot (which is every reader: the port only
enumerates once firmware runs) must not have to reset the board to
learn what is on it.

`Booted` is what boot leaves behind: the log, the LED, and its polarity.
`node::run` moves the log into the app's `Ctx` and keeps the LED for
the banner — unless `App::OWNS_STATUS_LED` says the app drives it.

## The boot timeline

```
reset ──► init() ──► B::wire ──► esp_rtos::start
                                   │
                                   ▼
                         boot::early ──► UsbLog (identify line)
                                   │    LED parked dark
                                   ▼
                    carriers::start ──► [LORA] state=up|down
                                   │
                                   ▼
                    peripherals::start ──► [BMV080]/[BME690] state=…
                                   │
                                   ▼
                    …node, wifi, app…
                                   │
                                   ▼
                         mainloop::run
                                   │  every pass:  [WIFI]/[LORA]/[PID] lines
                                   ▼
                        every 5 s:  banner (identify) + LED blink
```

A peripheral that cannot be configured (SPI or I2C refused) sends boot
down `halt_blinking`: blink + re-identify forever, instead of a silent
halt.

## Key points

- **The USB port IS the debug port.** No UART bridge on these boards —
  what a capture sees is what the log wrote.
- **Every line is stamped** with `t=<uptime_ms>`; the banner repeats
  every 5 s so a reader who attaches late never has to reset the board.
- **The LED is the no-cable boot proof** — parked dark, blinked at the
  banner, or owned by the app (Blink).
- **A broken bus blinks, a dead radio runs.** Only configuration
  failures are fatal (`halt_blinking`); absence of hardware is a log
  line.
