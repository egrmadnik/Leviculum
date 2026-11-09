# The boards modules (`src/boards/`)

The crate's per-board facts, one module per wiring: `xiao_s3.rs` and
`heltec_v4.rs`, plus `mod.rs` holding the shared `BoardConfig`.

## The split: per-class vs per-board

`boards/mod.rs` states the policy, and the line is drawn exactly along
"does this fact change when the board changes":

```
per class (lib.rs)          per board (boards/<board>.rs)
────────────────────        ─────────────────────────────────
init order                 which GPIO carries which net
clock configuration        the radio's pins and SPI clock
USB Serial/JTAG + banner   where the LED sits, which way it conducts
formatting/emitting log    battery divider, GNSS fitted?
the BoardConfig *shape*    the BoardConfig *values*, each constant
                           carrying the document it was read from
```

Adding a board is then: one `boards/<name>.rs` (pins + `CONFIG`), one
`src/bin/<name>.rs` (hands the pins to the shared init), one `[[bin]]`
stanza, one `Justfile` recipe. Nothing in `lib.rs` moves.

```
                     ┌───────────────────────────┐
                     │  lib.rs (per class)       │
                     │  init · clock · banner    │
                     └─────────────┬─────────────┘
                                   │ &'static BoardConfig
                     ┌─────────────▼─────────────┐
                     │  boards/mod.rs             │
                     │  struct BoardConfig        │
                     └──────┬──────────────┬──────┘
                            │              │
              ┌─────────────▼──┐   ┌───────▼──────────┐
              │ boards/xiao_s3 │   │ boards/heltec_v4 │
              │ pin aliases +  │   │ pin aliases +    │
              │ CONFIG         │   │ CONFIG           │
              └────────────────┘   └──────────────────┘
```

## `BoardConfig` — deliberately small

Five fields, each one a fact some *shared* function reads:

| field | read by |
|---|---|
| `log_prefix` | the `[BOARD]`/identity log lines |
| `board_name` | the boot banner |
| `lora_spi_freq_hz` | the radio's SPI configuration |
| `lora_max_power_dbm` | the radio's power ceiling |
| `gnss_onboard` | whether firmware may *assume* a receiver answers |

A constant only the board's own module uses stays in that module, next
to the schematic citation it came from — the nRF crate's `BoardConfig`
grew fields nobody consumed, and this one is the lesson.

## The compile-time pin contract

Every net is a named type alias over an `esp_hal` GPIO type. The bin
assigns through the alias, not by number:

```rust
let nss: xiao_s3::LoRaNss = p.GPIO41;   // src/bin/xiao_s3.rs
```

If the bin and the board file disagree about a net, the assignment
fails to compile. It is the only place the two can be held to each
other, and every alias carries the schematic/pinout source it was read
out of.

## `xiao_s3.rs` — the kit, in detail

Two Seeed modules mated by the kit's B2B connector: a plain XIAO
ESP32-S3 (R8: 8 MB flash, 8 MB PSRAM) and the kit-specific Wio-SX1262
adapter. The section headers are the board's anatomy:

```
boards/xiao_s3.rs
├── SX1262 LoRa radio, on SPI2     — 8 pin aliases + 4 constants
│     (LORA_SPI_FREQ_HZ · MAX_POWER_DBM · TCXO_ON_DIO3 · 1V8)
├── Indicators                     — LedPin (GPIO21, active LOW),
│                                     LED_ACTIVE_HIGH = false
├── Battery — none                 — no divider, no ADC pin
├── Optional L76K GNSS (add-on)    — Tx/Rx/standby aliases,
│                                     GNSS_ONBOARD = false
├── Header I2C (D4/D5)             — I2cSda (GPIO5) · I2cScl (GPIO6);
│                                     also the Qwiic OLED add-on's
│                                     home (`display` feature —
│                                     `node::display`, no reset net)
├── PID loop: DS18B20 + valve      — OnewireData (GPIO2) ·
│                                     Valve (GPIO4, D2 skipped:
│                                     GPIO3 is a strapping pin)
└── CONFIG                         — the BoardConfig value
```

### The radio nets live on the JTAG pads

Five of the eight radio nets land on pads a normal header footprint
cannot reach:

| radio net | pad | GPIO |
|---|---|---|
| NSS | MTDI | 41 |
| NRESET | MTMS | 42 |
| BUSY | MTDO | 40 |
| DIO1 | MTCK | 39 |
| RF-SW (RX enable) | FSPIWP | 38 |
| SCK · MISO · MOSI | D8 · D9 · D10 | 7 · 8 · 9 |

That is why Seeed says the compatible radio board "can only be bought
within the kit". The pads latch their strapping values at reset and are
free IO afterwards. Three independent sources agree on every net: the
kit pinout, the Meshtastic `seeed_xiao_s3` variant header, and the
RadioMesh Zephyr overlay.

### Decisions worth knowing

- **The kit is a bare SX1262** — no PA between chip and antenna, so
  `LORA_MAX_POWER_DBM = 22` is the air number, unlike the V4's front
  end.
- **The TCXO runs off DIO3 at 1.8 V** — the firmware must issue
  `SetDIO3AsTcxoCtrl` before calibration.
- **GPIO38 is the RX-enable, not DIO2.** Upstream's variant header
  writes `LORA_DIO2 38` and `SX126X_RXEN 38` for one physical net; the
  module resolves it like the Zephyr overlay: DIO2 stays the chip's
  internal TX steering, GPIO38 drives the antenna switch for RX.
- **The BOOT button is not a button** (it is the ROM download strap)
  and the battery pads are a charge input, not a sensed rail — so
  neither is declared.
- **D2 is skipped for the valve** because GPIO3 is a strapping pin: a
  valve that pulled the line at reset would write itself into the boot
  mode. The valve drives from GPIO4.

## `heltec_v4.rs` — the high-power variant

ESP32-S3R2 + SX1262 + a KCT8103L PA/LNA front end. Every net was
traced on the manufacturer's schematic (rev 4.2 and 4.3 both exist),
cited as `<sheet>: U7.<pin> <-> U9.<pin>`. The one open question the
module is honest about: the two schematic revisions **disagree about
who drives the front end's CTX and CPS** (DIO2 vs host GPIOs), so
those pins are declared `UNRESOLVED_*` with the evidence attached, and
the board revision must be read off the physical board before they are
wired. Step 1 puts nothing on the air and therefore does not need the
answer.

It also carries the only panel in the crate: the 0.96" SSD1315 OLED
(`OledSda`/`OledScl`/`OledReset` — GPIO17/18/21), driven by
`src/display.rs` under the `display` feature, plus the switched
battery divider (`BatterySensePin` GPIO1 + `BatterySenseEnable`
GPIO37 — the 390K/100K pair that puts VBAT/4.9 on the pin) that feeds
the screen's battery line.

## Key points

- **The board file is a citation list.** Every alias carries the
  document it was read out of; a claim can be checked at two pins,
  not trusted from a summary.
- **The seam is compile-checked.** Named aliases + `AnyPin` degrades
  mean the bin and the board file cannot silently drift apart.
- **One wiring per binary, not per SKU.** Two boards that agree on
  every net share one binary (board-support-scope policy).
- **What is absent is documented too** — no battery sense, no GNSS,
  no usable button, no second I2C — so nobody re-derives it.