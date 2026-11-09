# The board binaries (`src/bin/`)

One directory, four binaries, two architectures. The rule from
`architecture.md` — *a board binary is a hardware definition plus a
call to `node::run`* — is true of **`xiao_s3`**. The automation images
(`xiao_s3_auto`, `xiao_s3_echo`) are the exception that proves it:
they run `leviculum_automation`'s `App` directly with their own small
loop, because that framework predates the shared `node` seam. And
`heltec_v4` is the oldest of the four: a bring-up probe that holds the
radio's SPI port open and never transmits.

## The four, at a glance

| bin | board | architecture | app / behaviour |
|---|---|---|---|
| `heltec_v4` | Heltec V4 | standalone probe | none — boot, open the SX1262 SPI port, blink the banner. Nothing on the air |
| `xiao_s3` | XIAO S3 + Wio-SX1262 | `Board` + `node::run` | PID regulator (default) · echo inbox (`lxmf-echo`) · blinker (`lxmf-blink`) · WiFi carrier (`wifi`) |
| `xiao_s3_auto` | XIAO S3 + Wio-SX1262 | `leviculum_automation` standalone | thermostat: DS18B20 in, valve out, `AUTO` bench commands |
| `xiao_s3_echo` | XIAO S3 + Wio-SX1262 | `leviculum_automation` standalone | echo device: stores `ECHC` params, echoes them back |

Only the three XIAO bins share `boards::xiao_s3`; the Heltec bin uses
`boards::heltec_v4`. Each bin carries
`esp_bootloader_esp_idf::esp_app_desc!()` — without the descriptor
`espflash save-image` produces an image the ROM declines.

## How the pieces relate

```
                    ┌──────────────────────────────────────┐
                    │            src/node/ (shared)        │
                    │  board · boot · carriers · mainloop  │
                    │  app · responder · wifi · storage    │
                    └───────────▲──────────────────┬───────┘
                                │  Board impl      │  own loop
                    ┌───────────┴──────────┐       │
                    │  src/boards/         │       │
                    │  xiao_s3 · heltec_v4 │       │
                    └───────────▲──────────┘       │
                                │ named pin aliases│
   ┌───────────┬───────────────┴────┬──────────────┴───────────┐
   │ heltec_v4 │      xiao_s3       │   xiao_s3_auto/echo      │
   │ probe     │  Board + run       │   automation (own loop)  │
   │ (no air)  │  the LNode         │   thermostat · echo      │
   └───────────┴────────────────────┴──────────────────────────┘
```

## Build matrix

```sh
source ~/export-esp.sh
cd leviculum-esp

# The LNode — pick one app, optionally add wifi:
cargo build --release --bin xiao_s3                                   # PID
cargo build --release --bin xiao_s3 --features lxmf-echo              # echo inbox
cargo build --release --bin xiao_s3 --features lxmf-blink             # blinker
cargo build --release --bin xiao_s3 --features wifi                   # PID + WiFi

# The automation images (their own loop, no features):
cargo build --release --bin xiao_s3_auto
cargo build --release --bin xiao_s3_echo

# The probe:
cargo build --release --bin heltec_v4
```

## `xiao_s3` — the LNode, in detail

The bin is 117 lines: `const`s (name, config, LoRa profile, LED
polarity), a `wire` that moves pins out of `Peripherals` through the
board module's aliases, and one `#[esp_rtos::main]` that calls
`node::run::<XiaoS3>`. Everything else is shared code.

### The XIAO S3 pin map

```
        Seeed XIAO ESP32-S3 + Wio-SX1262 kit
   (pins are the board module's named aliases — the bin
    assigns each GPIO to one, so a mismatch fails to compile)

  radio (SPI2, via the B2B connector):
    NSS     GPIO41 (MTDI pad)     BUSY    GPIO40 (MTDO pad)
    RESET   GPIO42 (MTMS pad)     DIO1    GPIO39 (MTCK pad)
    RXEN    GPIO38 (FSPIWP pad)   SCK     GPIO7  (D8)
    MISO    GPIO8  (D9)           MOSI    GPIO9  (D10)
    SPI clock 8 MHz · TCXO on DIO3 at 1.8 V · 22 dBm bare-chip

  the rest of the board:
    USER_LED   GPIO21 (active LOW)
    I2C (Qwiic) SDA GPIO5 (D4) · SCL GPIO6 (D5)   ← BMV080 / BME690
    DS18B20     GPIO2 (D1, open-drain, 4.7 kΩ pull-up)
    valve       GPIO4 (D3, high opens)
    USB         GPIO19/20 (the SoC's own USB Serial/JTAG)
```

Five of the eight radio nets land on the **JTAG pads on the board's
underside** (MTCK/MTDO/MTDI/MTMS + FSPIWP) — which is why the
kit-specific Wio-SX1262 "can only be bought within the kit": a plain
header footprint cannot reach those pads. See `docs/boards.md`.

### The LoRa channel

```rust
freq 869.463 MHz · SF8 · BW 125 kHz · CR 4/5 · preamble 18 · 22 dBm
```

The fleet's `eu_medium` consensus numbers (ReticulumNet channel) — the
same figures `reticulum::register_interface` sizes the MTU 508 from.

### What each feature builds

```
  feature        app           note
  ─────────────  ────────────  ─────────────────────────────────
  (none)         pid_app       the PID valve regulator (default)
  lxmf-echo      echo          LXMF echo inbox (responder)
  lxmf-blink     blink         LED blinker; owns the status LED
  wifi           (any app)     + UDP/TCP interfaces 1 and 2,
                               runtime settings on the debug port
```

`lxmf-echo` + `lxmf-blink` together is a `compile_error!`.

### Flash sectors it touches

All through the shared `store` module — the identity at `0x701000`,
the PID tune at `0x700000`, the blink rate at `0x702000`, the WiFi
settings at `0x703000` (wifi feature). See `node/storage.md`.

### The Qwiic OLED (`display` feature)

The kit has no panel, but the Qwiic add-on pair (D4/D5) is the natural
home for an I2C OLED: with `--features display` the LNode drives it
from the main loop's banner tick — identity prefix, LoRa RX/TX
counters, heartbeat dot — over the same bus the sensors use. See
`node/display.md`.

## The automation images (`xiao_s3_auto`, `xiao_s3_echo`)

These two do not go through `node::run`. They are `leviculum_automation`
wrappers: a `main()` that wires pins → HAL adaptors (`automation.rs`),
brings up identity + `NodeCore` + the SX1262, and runs their own loop
(radio poll → `node.handle_packet` → `app.on_message` → `TickOutput`
→ air). Both share the `AUTO` bench-command path on the USB console
(`AUTO TARGET <32hex>`, `AUTO GET`, `AUTO LXMF <hex>`).

| | `xiao_s3_auto` | `xiao_s3_echo` |
|---|---|---|
| device | `Thermostat` | `Echo` |
| sensor / actuator | DS18B20 (D1) / valve (D3) | none (`NoIo`) |
| params store | flash sector `0x702000` | RAM only (echo data is not config) |
| LoRa profile | SF8 / 125 kHz (fleet channel) | SF7 / 250 kHz (fast local net) |
| announce name | `leviculum-xiao_s3-auto` | `leviculum-xiao_s3-echo` |

> **Caveat.** `xiao_s3_auto` chose its config sector (`0x702000`)
> before the shared node gained the blink slot at the same address.
> The two images are alternatives — one board runs one of them — so
> they never collide in practice, but if the automation image ever
> needs to share a board with the LNode firmware, its sector must
> move.

## `heltec_v4` — the probe

The first firmware this crate built. It proves the crate against real
hardware: SoC up, USB log open, the SX1262's SPI port constructed and
held (chip-select parked high, reset parked inactive, no opcode
issued), banner every 5 s. It is the ancestor of `xiao_s3`'s `wire` +
`node::run` shape, kept as a bring-up reference; nothing about it is on
the air.

With the `display` feature the V4's SSD1315 OLED becomes the bench
board's face: the same `leviculum-screen` status model the nRF boards
paint (title, build sha, battery from the switched divider, heartbeat
dot), refreshed at the banner cadence — and driven even when the radio
bus refused to configure, so a dead radio and a live screen is a
diagnosable board. The driver lives in `src/display.rs`; the panel is
the reason the `display` feature exists (the XIAO kit has none).

```sh
cargo build --release --features display --bin heltec_v4
```

## Key points

- **`xiao_s3` is the shared seam's poster child**: pins in the bin,
  behaviour in `src/node`, app chosen by feature.
- **The automation bins predate the seam** and run their own loop —
  the same calls, inlined rather than shared.
- **The pin contract is compile-time.** `let nss: xiao_s3::LoRaNss =
  p.GPIO41` fails to compile when the bin and the board file disagree.
- **Every image announces who it is** (`leviculum-xiao_s3-*`), so the
  fleet can tell them apart in `rnpath`/`rnstatus`.