# The shared LNode seam (`src/node/`)

The mirror of `leviculum-esp/src/node`: one firmware, many boards. A
board binary is a `Board` implementation plus a call to
[`run`]; everything an LNode does that does not depend on the unit it
runs on lives here, split by function in boot order:

| module | what it owns |
|---|---|
| `board` | the seam: pin map, rails, per-board hooks |
| `boot` | diagnostics, persisted records, USB, boot banner |
| `reticulum` | identity, the transport node, the three interfaces |
| `carriers` | the LoRa radio **and the BLE peripheral** |
| `storage` | the internal-flash stores |
| `peripherals` | GNSS and battery |
| `readings` | telemetry readings from the sensors above |
| `mainloop` | the cooperative event loop |
| `tasks` | housekeeping banners |
| `responder` | the shared LXMF inbox (`lxmf-echo`/`lxmf-blink`) |
| `blink` · `echo` | the two LXMF apps (feature-selected) |

## How it differs from the ESP mirror

Same shape, different hardware:

```
leviculum-esp/src/node              leviculum-nrf/src/node
──────────────────────              ──────────────────────
carriers: LoRa only                 carriers: LoRa + BLE (Columba)
reticulum: 1-2 interfaces           reticulum: 3 interfaces (USB/LoRa/BLE)
mainloop: ~120 lines                mainloop: ~40 KB (PN, telemetry,
                                    usb control envelope, media)
storage: 4 flash slots              storage: record log + radio store
peripherals: I2C sensors            peripherals: GNSS + battery
```

The nRF side is the older, larger sibling: the propagation-node
engine (`pn.rs`), telemetry (`telemetry.rs`) and the USB control
envelope live in `src/` and are driven from `mainloop`, while the ESP
side is the slimmed-down re-plant.

## The app seam

Exactly like the ESP side: `App` + `Ctx` in `mainloop`, a `DefaultApp`
resolved by feature.

```
  (none) ──► announce-only node (delivery destination, no inbox)
  lxmf-echo ──► node::echo — full LXMF inbox via responder
  lxmf-blink ──► node::blink — LED blinker (owns the LED task)
      lxmf-echo + lxmf-blink ──► compile_error!
```

## Boot (from `run`)

```
boot::init ──► B::wire ──► boot::early ──► board.power_up
   ──► reticulum::build (identity + transport)
   ──► register_interfaces (USB 0 · LoRa 1 · BLE 2)
   ──► board.qspi ──► carriers::start_lora
   ──► carriers::start_ble (SoftDevice + Columba)
   ──► storage::start ──► board.spawn_display
   ──► peripherals::start (GNSS, battery)
   ──► board.boot_indicator ──► mainloop::run
```

## Key points

- **The board binary stays thin** — pins in `boards/`, everything else
  in `node/` (and the policy crates beyond it).
- **BLE is a carrier, not a feature** — `carriers::start_ble` is as
  much a carrier as LoRa; the transport sees interface 2.
- **The app pick is feature-driven** (`lxmf-echo` / `lxmf-blink`,
  mutually exclusive), same as the ESP side — the loop, carriers and
  transport are untouched by which app runs.