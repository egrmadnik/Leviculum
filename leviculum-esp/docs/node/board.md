# `node::board` — the hardware seam

`src/node/board.rs`

The `Board` trait carries everything that differs between units and
nothing else. A board binary (e.g. `src/bin/xiao_s3.rs`) implements it
and calls `node::run`; all behaviour lives in the sibling modules.

```rust
trait Board {
    const NAME: &'static str;            // banner name
    const CONFIG: &'static BoardConfig;  // runtime [BOARD] metadata
    const LORA: LoraProfile;             // the channel the fleet agrees on
    const LED_ACTIVE_HIGH: bool;         // status LED polarity
    fn wire(p: Peripherals) -> Wiring;   // pin split — touches no hardware
}
```

On the XIAO S3: `LORA` is the fleet consensus channel —
869.463 MHz, SF8, BW 125 kHz, CR 4/5, 18-symbol preamble, 22 dBm — and
`LED_ACTIVE_HIGH = false` (the yellow USER_LED on GPIO21 is active low).

## Wiring types

`wire()` returns a `Wiring` of five bundles:

| field | type | contents |
|---|---|---|
| `system` | `System` | USB_DEVICE, FLASH, RNG+ADC1 (the TRNG needs both), TIMG0+FROM_CPU_INTR0 (esp-rtos tick and software interrupt), WIFI |
| `led` | `AnyPin` | the status LED |
| `lora` | `LoraWiring` | SPI2 + SCK/MOSI/MISO/NSS/BUSY/RESET/DIO1/RXEN and the SPI clock |
| `sensors` | `SensorWiring` | I2C0 + SDA/SCL for the add-on sensors |
| `app` | `AppWiring` | the app's pins: `onewire`, `valve`, and the status LED *if the app owns it* (filled in by `run`, left `None` by the board) |

Pins travel as type-erased `AnyPin`s. The compile-time check that
binary and board file agree is in `wire` itself:

```rust
let nss: xiao_s3::LoRaNss = p.GPIO41;
```

assigning each GPIO to a named alias fails to compile if either side
moves a net.

## What `wire` hands out

```
 board binary (xiao_s3.rs)                  Board trait
 ─────────────────────────                  ───────────
 const NAME · CONFIG · LORA · LED_ACTIVE_HIGH
 fn wire(p: Peripherals) ──► Wiring
                              ├─ system ─ usb · flash · rng+adc1 (TRNG)
                              │           timg0+sw_int0 (tick) · wifi
                              ├─ led    ─ AnyPin: the status LED
                              ├─ lora   ─ SPI2 + sck/mosi/miso/nss/
                              │           busy/reset/dio1/rxen
                              ├─ sensors─ I2C0 + sda/scl (Qwiic pair)
                              └─ app    ─ onewire · valve · led (if the
                                          app owns it, filled by run)
```

Everything downstream consumes `Wiring`; only the two bins and the
board module ever touch GPIO numbers.

## Key points

- **The seam is one trait.** A new board is a new `Board` impl plus a
  `node::run` call — no shared code changes.
- **`wire` touches no hardware.** It is pin bookkeeping; the modules
  that open the peripherals are where the errors surface.
- **Named pin aliases are the contract.** `let nss: xiao_s3::LoRaNss =
  p.GPIO41` fails to compile when the binary and the board file
  disagree about a net.
