# What lives in `src/`

The firmware crate's own tree: `src/` (class init + the big drivers),
`src/bin/`, `src/boards/`, `src/ble/`, `src/node/`. The rule from
`lib.rs` — nothing here is host-tested — is why the *big* files are
drivers and the *small* files call member crates.

```
src/
├── lib.rs            class init: heap, panic, USB log, BSP compile_error!
├── announce.rs       announce handling (peers, cadence, rate limits)
├── automation.rs     leviculum_automation HAL adaptors (sensor/actuator)
├── battery.rs        SAADC battery readout (scale in battery-scale)
├── baseboard.rs      RAK19026 baseboard peripherals
├── ble/              the BLE carrier — see below
├── bin/              the eight binaries — see docs/bin.md
├── boards/           the four pin-map modules — see docs/boards.md
├── boot_count.rs     flash-persisted reset counter (logic in boot-count)
├── boot_trace.rs     noinit-RAM breadcrumbs (layout in boot-trace)
├── button.rs         user-button handling
├── clock.rs          uptime clock for the transport
├── dispatch.rs       TickOutput → per-interface air
├── display.rs        SSD1306 status display (frames from leviculum-screen)
├── events.rs         node-event mapping (announce/link/message)
├── flash.rs          internal-flash helpers
├── gnss.rs           L76K driver (policies in gnss-* crates)
├── heap_census.rs    heap used/free accounting
├── identity.rs       identity load/generate + announce data
├── iface_bytes.rs    per-interface byte counters
├── interface.rs      the transport Interface impl glue
├── led.rs            status LED
├── log.rs            debug log over USB CDC-ACM (shape in log-line)
├── lora.rs           106 KB — the LoRa radio driver + TX path
├── media.rs          media-state reporting (policy in media-state)
├── mgmt.rs           management frames on the control interface
├── name.rs           device naming
├── node/             the shared LNode seam — see docs/node.md
├── pn.rs             139 KB — the propagation-node engine
├── qspi.rs           54 KB — QSPI NOR driver (solarnode)
├── radio_store.rs    persisted LoRa configuration page
├── record_store.rs   the record-log store over internal flash
├── remote_status.rs  status queries from peers
├── rng.rs            RNG plumbing through the SoftDevice
├── st7789.rs         T114 display backend
├── store_spike.rs    #384 size harness (sequential-storage spike)
├── sx1262.rs         74 KB — the SX1262 chip driver
├── telemetry.rs      93 KB — telemetry reporter + target lifecycle
├── transport_stats.rs transport counters for status lines
├── usb.rs            69 KB — USB CDC-ACM device + control envelope
└── vext.rs           VEXT rail control (T114)
```

## `src/ble/` — the BLE carrier, split along the protocol seam

```
src/ble/
├── mod.rs       protocol-neutral: SoftDevice bring-up + RAM floor
│                guard, Irqs, SoC-event task, packet channels, the
│                Interface impl, per-link queues and fan-out, the
│                fragment pump (walks leviculum_ble_tx::PacketTx)
├── columba.rs   the Columba v2.2 protocol and nothing else: GATT
│                service/UUIDs, 16-byte identity handshake, 1-byte
│                keepalive, advertisement contents (v0.3.0 capability
│                record), the MAC-sorting connection rule
└── notify.rs    per-connection HVN drain table
```

The acceptance test for the split (#255): a sibling `ble_leviculum`
carrier could be added without touching `columba.rs`.

## `src/node/` — the shared LNode seam

See `docs/node.md` — the mirror of `leviculum-esp/src/node` that runs
the same transport on this hardware, with BLE as interface 2.

## `tools/` — host-side tooling (not firmware)

```
tools/
├── bin2uf2.rs            flat binary → UF2 (zero deps, rustc-compilable)
├── uf2-runner.sh         the cargo runner: build → UF2 → copy to drive
├── flash-and-read.sh     flash + capture the debug log
├── fw-readback.sh        read the image back off a board
├── flash-attribution.sh  which commit flashed this board
├── debug-read.sh         debug-port capture helper
├── softdevice-guard.sh   / test-softdevice-guard.sh
├── uf2-volumes.sh        / test-uf2-volumes.sh    bootloader-drive scan
├── store-spike-size.sh   size harness helper
└── test-fw-readback.sh   readback test
```

## Key points

- **Small logic, big drivers.** The largest files are hardware
  drivers (`pn.rs`, `lora.rs`, `telemetry.rs`, `usb.rs`, `sx1262.rs`).
  Policy files are thin wrappers over member crates.
- **`lib.rs` enforces the board select** — exactly one `bsp-*` feature,
  at compile time, so a binary that forgets to pick a board fails
  `cargo build`, not on the bench.
- **`ble/` is a protocol seam**: the wire protocol and the machinery
  under it are separate modules with a stated acceptance test.