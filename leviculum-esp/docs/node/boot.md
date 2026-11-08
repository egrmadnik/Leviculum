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
