# Architecture — nRF52840 family

`leviculum-nrf` is Reticulum firmware for four nRF52840 boards: the
Heltec Mesh Node T114, the WisMesh Pocket V2 (RAK4631), the SenseCAP
Solar Node P1-Pro, and the Seeed XIAO nRF52840 + Wio-SX1262 kit. It is
the sibling of `leviculum-esp` (ESP32-S3 boards) and shares the same
`leviculum-core` transport engine; where the ESP crate is one crate,
this one is a 30-crate workspace — because the nRF side made
host-testability a first-class architectural line.

## The whole shape

```
┌───────────────────────────────────────────────────────────────────┐
│ APPLICATION — on top of the LNode (feature or bin)                │
│   announce-only node (default) · LXMF echo inbox · LXMF blinker  │
│   automation devices: lxmf_echo, auto_echo                       │
├───────────────────────────────────────────────────────────────────┤
│ SHARED NODE SEAM — src/node/ (mirror of leviculum-esp)            │
│   board · boot · carriers · mainloop · peripherals · readings ·   │
│   reticulum · storage · tasks · responder · blink · echo          │
├───────────────────────────────────────────────────────────────────┤
│ TRANSPORT — NodeCore (Reticulum): identity, destinations, links,  │
│             announces, forwarding (transport mode on)             │
├───────────────┬──────────────────────┬────────────────────────────┤
│ iface 0       │ iface 1              │ iface 2                    │
│ USB CDC-ACM   │ SX1262 LoRa          │ BLE peripheral             │
│ (HDLC to host)│ (lora.rs + sx1262.rs)│ (ble/: Columba v2.2 +      │
│               │                      │  protocol-neutral machinery)│
├───────────────┴──────────────────────┴────────────────────────────┤
│ EMBASSY EXECUTOR (thread mode) over the S140 SoftDevice            │
│   nrf-softdevice: BLE + Flash via syscalls; SoftDevice owns the    │
│   radio/PPI/RNG registers — direct access panics (sd-policy)       │
├───────────────────────────────────────────────────────────────────┤
│ embassy-nrf · embassy-usb · nrf52840 · QSPI NOR (solarnode) ·     │
│   internal flash regions (memory.x) · flip-link · heap census     │
└───────────────────────────────────────────────────────────────────┘
```

## The host-test seam — the defining decision

`src/lib.rs` states it as policy: **nothing in this crate is tested.**
The firmware cross-compiles for `thumbv7em-none-eabihf`, both BSP
paths go through `nrf-softdevice-s140`, which does not compile for a
host triple — so every `#[test]` in the workspace lives in one of the
29 pure member crates beside it.

```
                    ┌─────────────────────────────┐
                    │  src/ (the firmware crate)  │
                    │  takes pins, registers,     │
                    │  syscalls, Instants         │
                    └──────┬──────────────┬───────┘
                           │ calls        │ calls
              ┌────────────▼─────┐  ┌─────▼─────────────┐
              │ pure member      │  │ pure member       │
              │ crates (29):     │  │ crates (29):      │
              │ sd-policy        │  │ channel-access    │
              │ telemetry-policy │  │ tx-spacing        │
              │ announce-policy  │  │ screen            │
              │ …                │  │ …                 │
              └──────────────────┘  └───────────────────┘
              each runs `cargo test` on the host: the full
              decision table, the golden frame, the fake clock
```

A *decision* — a cadence, a threshold, an ordering, a predicate, an
arithmetic budget, a byte-exact line other tools grep — belongs in a
member crate. What stays in `src/` is what takes hardware. The policy
and the seam's current extent are tracked in
`docs/src/concepts/firmware-host-test-seam.md` (repo root).

## Execution model

- **Embassy executor** (thread mode, `executor-thread`), time driver on
  RTC1, GPIO events via GPIOTE.
- **The S140 SoftDevice** runs BLE and owns the radio/PPI/RNG/POWER
  registers. `nrf-softdevice` provides Rust-async wrappers and a
  SoftDevice-aware `critical_section` impl. This replaced a Day-1
  nrf-mpsl stack that had a documented reentrance bug (#32).
- **The heap** is `embedded-alloc`; `heap_census.rs` reports used/free.
- **The linker** is `flip-link` (stack at the bottom of RAM so an
  overflow faults against the RAM floor); `memory.x` lays out flash,
  RAM and the reserved noinit/flash regions.
- **The runner** is `tools/uf2-runner.sh` — `cargo run` builds, converts
  to UF2 (`tools/bin2uf2.rs`), and copies onto the board's bootloader
  drive.

## Interfaces

| InterfaceId | medium | framing | notes |
|---|---|---|---|
| 0 | USB CDC-ACM | HDLC | the debug/control port to a host |
| 1 | SX1262 LoRa | RNode split frames | `lora.rs` + `sx1262.rs` (73–106 KB of driver) |
| 2 | BLE peripheral | Columba v2.2 GATT | `ble/`: protocol vs machinery split (#255) |

Transport mode is on: the node forwards between all three interfaces —
a host on USB talks to the LoRa mesh and to BLE peers alike.

## Boot order

`node::run::<B>(spawner)` wires the system end to end:

```
boot::init ──► B::wire ──► boot::early (USB, records, banner)
   │              │
   ▼              ▼
board.power_up ──► reticulum::build (identity, transport)
   │              │
   ▼              ▼
board.qspi ──► carriers::start_lora (SX1262, continuous RX)
   │              │
   ▼              ▼
carriers::start_ble (SoftDevice + Columba peripheral)
   │              │
   ▼              ▼
storage::start (flash stores) ──► board.spawn_display
   │              │
   ▼              ▼
peripherals::start (GNSS, battery) ──► boot_indicator
   │
   ▼
mainloop::run (forever)
```

## Feature flags (the board select)

```
  bsp-t114 ──► Heltec Mesh Node T114      (display + gnss + battery)
  bsp-rak4631 ──► WisMesh Pocket V2       (rak-baseboard adds display)
  bsp-solarnode ──► SenseCAP Solar Node   (battery + gnss, no display)
  bsp-xiaokit ──► XIAO nRF52840 kit       (battery + gnss, no display)
      exactly one, enforced by compile_error! in lib.rs

  app:    lxmf-echo · lxmf-blink          (mutually exclusive)
  add-ons: gnss · battery · display · rak-baseboard
  instruments: qspi-selftest
```

## Where the packets flow

```
ingress:  LoRa ──► SplitReassembler ──► node.handle_packet(1, …)
          BLE ──► ble channels ──► node.handle_packet(2, …)
          USB ──► HDLC deframe ──► node.handle_packet(0, …)
          every result ──► app.on_events → dispatch

egress:   TickOutput ──► per-interface dispatch
          USB/HDLC · LoRa frames · BLE fan-out (per-link queues,
          broadcast on every live link)
```

## Key points

- **One firmware, four boards, one binary per board** — the BSP
  features select the wiring; the shared code is untouched.
- **Everything testable is outside `src/`** — the seam is the crate
  boundary, not a comment.
- **The SoftDevice is the platform** — BLE and flash go through
  syscalls; the policy for what to do when a syscall fails lives in
  `sd-policy`, not in a register trap.
- **BLE is a protocol seam, not a pile** — `ble/columba.rs` is the
  wire protocol; `ble/mod.rs` + `ble/notify.rs` are the
  protocol-neutral machinery, so a second BLE carrier can replace the
  protocol without touching the transport.