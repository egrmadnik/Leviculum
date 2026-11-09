# The binaries (`src/bin/`)

Eight binaries, three kinds: **board LNodes** (a `Board` impl + the
shared `node::run`), **app variants** (the same board with a different
application), and **instruments** (measurement firmware, not for the
mesh).

## The eight, at a glance

| bin | kind | board | what it runs |
|---|---|---|---|
| `t114` | board LNode | T114 | the transport node: USB + LoRa + BLE |
| `rak4631` | board LNode | RAK4631 | same, on the WisMesh Pocket V2 |
| `solarnode` | board LNode | SolarNode P1-Pro | same + QSPI NOR + GNSS + battery |
| `xiaokit` | board LNode | XIAO nRF52840 kit | same, on the XIAO kit |
| `lxmf_echo` | app variant | XIAO kit | full LXMF echo inbox (`LxmfNode::register`) |
| `auto_echo` | app variant | XIAO kit | `leviculum_automation` Echo device (RAM store) |
| `qspi-selftest` | instrument | SolarNode | erases + tests the external flash (!!) |
| `sd-ram-probe` | instrument | any | measures the SoftDevice RAM requirement curve |

## Build

Every bin is built against exactly one `bsp-*` feature (the board
select):

```sh
cargo build -p leviculum-nrf --release --features bsp-t114 --bin t114
cargo build -p leviculum-nrf --release --features bsp-rak4631 --bin rak4631
cargo build -p leviculum-nrf --release --features bsp-solarnode --bin solarnode
cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin xiaokit
cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin lxmf_echo
cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin auto_echo
```

Flashing goes through the Justfile recipes (`just flash-t114`,
`flash-rak4631`, `flash-solarnode`, `flash-xiaokit`, …), which wrap
`cargo run`; the `.cargo/config.toml` runner converts the ELF to UF2
(`tools/bin2uf2.rs`) and copies it onto the bootloader drive.

## The board LNodes — what `t114` says about itself

```
Interface 0: USB CDC-ACM serial (HDLC framing) to host
Interface 1: SX1262 LoRa radio
Interface 2: BLE peripheral (Columba v2.2 protocol)
The transport engine routes packets between all interfaces.
```

That is the whole job, on every board: bring the transport node up
with three media, announce, and let the mesh move. The boards differ
in what the shared code wires to them — see `docs/boards.md`.

## The app variants (XIAO kit)

- **`lxmf_echo`** — registers the *full* `lxmf.delivery` inbox
  (`LxmfNode::register`: packet + resource delivery, direct links) and
  answers every message with `echo: <text>`. No telemetry reporter, no
  PN engine, no GNSS/battery — the smallest complete LXMF inbox.
- **`auto_echo`** — the `leviculum_automation` Echo device: a remote
  node sends an `ECHC` params frame (≤ 32 bytes) in a signed
  opportunistic LXMF message; the board applies it and the next `ECHS`
  report returns the same bytes. Params live in RAM (echo data is not
  config, so no flash wear). The whole LXMF round trip is tested in
  `leviculum-automation` with one call.

## The instruments

- **`qspi-selftest`** — SolarNode external-flash self-test (OTA stage
  2): erase, program, read back the whole P25Q16H with timings.
  **This binary erases the whole external flash and re-runs on every
  boot.** No bundle or `lnflash` entry carries it; only
  `just flash-solarnode-qspi-selftest` flashes it.
- **`sd-ram-probe`** — asks the S140 what application RAM start each
  candidate BLE configuration requires, printing the cost curve over
  the debug CDC. It never advertises, scans or starts the mesh: each
  measurement is enable → config series → enable against an
  undersized RAM base → read the required base out of the
  `NRF_ERROR_NO_MEM` reply → disable.

## Key points

- **Four boards, one shape.** The board LNodes are `Board` impls; the
  app variants reuse the same board file with a different payload.
- **Instruments never ship** — no bundle, no `lnflash` entry; the
  self-test even warns in its own doc comment.
- **The app seam is feature-driven** (`lxmf-echo`/`lxmf-blink` on the
  shared node) *and* bin-driven (the automation wrappers) — both live
  beside the board LNodes.