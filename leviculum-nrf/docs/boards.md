# The boards modules (`src/boards/`)

Four board modules — `t114`, `rak4631`, `solarnode`, `xiaokit` — plus
`mod.rs` holding the shared `BoardConfig`. Same pattern as
`leviculum-esp/src/boards`, with one extra axis: the **antenna switch**
is a two-shaped thing, and `boards/mod.rs` explains it out loud.

## The per-class / per-board split

```
per class (lib.rs, src/)          per board (boards/<board>.rs)
──────────────────────────        ─────────────────────────────────
init order, clock, USB, banner   which GPIO carries which net
the BoardConfig *shape*          the BoardConfig *values*
the transport, carriers, loop    the front-end shape (see below)
```

A board module exposes: pin type aliases (the compile-time contract —
the bin assigns `let nss: t114::LoRaNss = p.P0_13;` and a mismatch
fails to compile), hardware constants with citations, and
`pub const CONFIG: BoardConfig`.

## `BoardConfig` — the metadata shared init reads

| field | read by |
|---|---|
| `usb_vid` / `usb_pid` / `usb_manufacturer` / `usb_product` | the USB CDC-ACM device |
| `log_prefix` | identity log lines ("T114", "RAK", …) |
| `identity_flash_page` | the identity page |
| `radio_config_flash_page` | the persisted LoRa configuration |
| `telemetry_flash_page` | telemetry target + fixed position |
| `lora_tcxo_voltage_reg` | `SetDIO3AsTcxoCtrl` (0x02 = 1.8 V) |
| `lora_spi_freq_hz` / `lora_max_power_dbm` | the radio's SPI and power |
| `qspi_part` | the fitted NOR part (`None` = no part mounted) |

The three flash pages sit just below `0xEA000` — the bootloader's
`USER_FLASH_END`, so a UF2 flash declines every block from there
upwards and the configuration survives a firmware update.

## The antenna switch: one boolean is not enough

Every board sets `SetDIO2AsRfSwitch` — the SX1262 raises DIO2 for a
transmit and the board's switch follows it. That is the *whole* story
on the T114 and the RAK4631 (whose antenna-switch pad exists on the
schematic and must NOT be driven — DIO2 owns it).

The Wio-SX1262 (on the solar node's carrier and the XIAO kit) splits
the job: DIO2 still steers TX, but **RX is a host GPIO**
(`LoRaRxEnable`) that must be asserted for a listening window and
released before key-up — with both asserted at once the switch is in
neither position and the transmit goes nowhere.

The RX-enable pin is declared as a type alias like every other pin and
handed to `crate::lora::init` as `Some(..)`. It is deliberately not
mirrored as a `BoardConfig` flag: the driver takes the pin itself, so
a second copy of the fact could only disagree.

## The four boards

| module | board | front end | extras |
|---|---|---|---|
| `t114` | Heltec Mesh Node T114 | DIO2 only | ST7789 display, GNSS, battery, VEXT |
| `rak4631` | WisMesh Pocket V2 | DIO2 only | `rak-baseboard` adds SSD1306 display |
| `solarnode` | SenseCAP Solar Node P1-Pro | DIO2 + host RX-enable | QSPI NOR (P25Q16H), GNSS, battery |
| `xiaokit` | Seeed XIAO nRF52840 + Wio-SX1262 | DIO2 + host RX-enable | GNSS, battery |

## Key points

- **The pin contract is compile-time**: named aliases + `AnyPin`
  degrade; the bin and the board file cannot silently drift.
- **`qspi_part` decides the QSPI story.** `None` means no part is
  mounted — and then nothing may configure the QSPI pins at all (the
  T114/RAK nets are on the expansion header, two of them dual-use).
- **The front end is the interface's business** — the RX-enable
  switching lives in `sx1262.rs`/`lora.rs`, not in `transport.rs`
  (interface-isolation policy).