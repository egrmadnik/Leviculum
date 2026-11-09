# `leviculum-nrf` documentation

Reticulum firmware for nRF52840-based boards: the Heltec Mesh Node
T114, the WisMesh Pocket V2 (RAK4631), the SenseCAP Solar Node
P1-Pro, and the Seeed XIAO nRF52840 + Wio-SX1262 kit.

```
┌───────────────────────────────────────────────┐
│  README.md        ← you are here (the index)  │
│  architecture.md  ← the whole system, top-down│
│  workspace.md     ← the 30-crate workspace    │
│                   (the host-test seam)        │
│  src.md           ← what lives in src/        │
│  bin.md           ← the eight binaries        │
│  boards.md        ← the four pin-map modules  │
│  node.md          ← the shared LNode seam     │
└───────────────────────────────────────────────┘
```

## Where to start

| read this | to understand |
|---|---|
| `architecture.md` | the layered stack, execution model, interfaces, the host-test seam in one picture |
| `workspace.md` | why the crate is a workspace: the 29 pure, host-testable member crates and what each decides |
| `src.md` | the top-level `src/` layout: `lib.rs`, the big driver modules, `ble/`, `tools/` |
| `bin.md` | the eight binaries: four board LNodes, two app variants, two instruments |
| `boards.md` | the per-board pin modules, `BoardConfig`, the antenna-switch story |
| `node.md` | the shared `node/` seam, mirror of `leviculum-esp/src/node` |

## The architecture in five lines

1. **A board binary is a `Board` impl plus `node::run`** — pins in
   `src/boards`, behaviour in `src/node`, one of four `bsp-*` features
   selects the board (exactly one, enforced by `compile_error!`).
2. **Embassy on the SoftDevice.** The executor is embassy-executor
   over `embassy-nrf`; every board runs the S140 SoftDevice (via
   `nrf-softdevice`), which owns the radio/BLE registers — hence
   `sd-policy`, and the panic-on-trap discipline it encodes.
3. **Three transport interfaces, one node**: USB CDC-ACM (0), LoRa
   SX1262 (1), BLE peripheral running the Columba v2.2 protocol (2).
   Transport mode forwards between them.
4. **Nothing in the firmware crate is host-tested.** Every decision a
   host could assert — cadences, thresholds, orderings, byte-exact log
   lines — lives in one of the 29 pure member crates; `src/` calls them.
5. **On top of the LNode sits an app** — the PID-style announce-only
   node, or the LXMF echo inbox / blinker (`lxmf-echo`/`lxmf-blink`),
   or a `leviculum_automation` device (`lxmf_echo`, `auto_echo` bins).

## Reading conventions

Every page ends with **Key points**. ASCII diagrams show ownership and
flow; code pointers are `src/…:line` style so the source stays the
ground truth.