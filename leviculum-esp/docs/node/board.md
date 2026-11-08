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
