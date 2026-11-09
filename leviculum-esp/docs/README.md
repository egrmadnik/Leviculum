# `leviculum-esp` documentation

The firmware for the ESP32-S3 LNodes (XIAO ESP32-S3 + Wio-SX1262,
Heltec V4): Reticulum on LoRa with optional WiFi, one application per
build.

```
┌───────────────────────────────────────────────┐
│  README.md        ← you are here (the index)  │
│  architecture.md  ← the whole system, top-down│
│  build-and-flash.md ← toolchain → .bin → board│
│  bin.md           ← the four board binaries   │
│  boards.md        ← the pin-map modules       │
│  node/            ← one file per module       │
└───────────────────────────────────────────────┘
```

## Where to start

| read this | to understand |
|---|---|
| `architecture.md` | the layered stack, boot order, interfaces, packet flow, feature flags, flash map |
| `build-and-flash.md` | the `esp` toolchain, `cargo build --release`, `espflash save-image`/`flash`, `just build-esp32` |
| `bin.md` | the four binaries — `xiao_s3` (the LNode), `xiao_s3_auto`, `xiao_s3_echo`, `heltec_v4` — and the full XIAO S3 pin map |
| `boards.md` | the per-board pin modules: the class/per-board split, `BoardConfig`, the compile-time pin contract |
| `node/board.md` | the `Board` seam: one trait, five wiring bundles |
| `node/boot.md` | the debug log, banner cadence, halt-vs-degraded policy |
| `node/carriers.md` | the LoRa carrier: RX/TX paths, interface 0 |
| `node/reticulum.md` | identity persistence and the `NodeCore` transport |
| `node/storage.md` | the shared flash handle and the sector map |
| `node/app.md` | the `App`/`Ctx` seam, LXMF via `responder`, the three apps |
| `node/responder.md` | the shared LXMF inbox: message lifecycle, send policy |
| `node/mainloop.md` | the cooperative event loop, one pass at a time |
| `node/peripherals.md` | the I2C add-on sensors (BMV080, BME690) |
| `node/wifi.md` | the WiFi carrier: tasks, channels, runtime settings |
| `node/display.md` | the LNode's status screen (Qwiic OLED add-on) |

## The architecture in five lines

1. A **board binary** is a `Board` impl (pin map, LoRa channel) plus
   `node::run` — everything shared lives in `src/node`.
2. One **cooperative main loop** owns the transport node and polls
   LoRa, WiFi and the debug port; **Embassy tasks** overlap what must
   (LED, flash, sockets).
3. **Three transport interfaces**, one node: LoRa (0), WiFi UDP (1),
   WiFi TCP (2) — transport mode makes the node a live bridge.
4. The **app is a plug** (`DefaultApp` by feature); it talks to the
   mesh through the shared LXMF `responder` — command in, reply out.
5. Everything remote-set — identity, PID tune, blink rate, WiFi
   settings — **survives reset** in its own flash sector.

## Reading conventions

- Every module doc ends with **Key points** — the load-bearing ideas.
- ASCII diagrams show data flow and ownership; code pointers are
  `src/…:line` style so the source stays the ground truth.