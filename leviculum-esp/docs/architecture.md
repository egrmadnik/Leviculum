# LNode architecture — XIAO ESP32-S3

An **LNode** is a Reticulum node on battery hardware: an SX1262 LoRa
radio, a persistent identity, and one application. The XIAO ESP32-S3 +
Wio-SX1262 kit is one board that runs it; the same shared code runs on
the Heltec V4 and, with different drivers, on the nRF boards
(`leviculum-nrf/src/node`, which this tree mirrors).

The design rule is one sentence: **a board binary is a hardware
definition plus a call to `node::run`; everything else lives in
`src/node`.**

```
src/bin/xiao_s3.rs        Board impl: pin map, LoRa channel, LED polarity
src/boards/xiao_s3.rs     named pin aliases + BoardConfig (banner text)
src/node/                 all shared behaviour, split by function
```

The board files and binaries are documented in full in `docs/bin.md`
(four binaries, two architectures, the XIAO S3 pin map) and
`docs/boards.md` (the class/per-board split, `BoardConfig`, the
compile-time pin contract).

## The whole shape

```
┌────────────────────────────────────────────────────────────────────┐
│ APPLICATION — one of three, picked by feature (App trait)          │
│   pid_app (default) · echo (lxmf-echo) · blink (lxmf-blink)        │
│   on_events / on_line / tick      spawns its own Embassy tasks     │
├────────────────────────────────────────────────────────────────────┤
│ APP SEAM — App + Ctx (node, carrier, log, flash, identity,         │
│             spawner); responder = LXMF inbox for the apps          │
├────────────────────────────────────────────────────────────────────┤
│ COOPERATIVE MAIN LOOP — mainloop::run                              │
│   sensors → radio ingress → node timers → wifi ingress → commands  │
│   → app.tick → sleep 100 ms → banner every 5 s                     │
├────────────────────────────────────────────────────────────────────┤
│ TRANSPORT — NodeCore (Reticulum): identity, destinations, links,   │
│             announces, forwarding (transport mode on)              │
├───────────────┬──────────────────────────────┬─────────────────────┤
│ LoRa carrier  │  WiFi carrier (feature)      │  LXMF responder     │
│ iface 0       │  iface 1 UDP · iface 2 TCP   │  (lxmf-echo/blink)  │
│ SX1262        │  esp-radio + embassy-net     │                     │
├───────────────┴──────────────────────────────┴─────────────────────┤
│ EMBASSY EXECUTOR (esp-rtos) — the async tasks the loop spawns:     │
│   app tasks (led, persist) · wifi tasks (connection, net, udp, tcp)│
├────────────────────────────────────────────────────────────────────┤
│ esp-hal — SPI · I2C · GPIO · 1-Wire · USB Serial/JTAG · TRNG ·     │
│           TIMG0 tick + software interrupt · internal flash         │
└────────────────────────────────────────────────────────────────────┘
```

Two concurrency styles, deliberately different:

- **One async main loop** owns the transport node, the log and the
  sensors. It *cooperatively polls* — nothing in it may block.
- **Embassy tasks** (spawned by the app or the WiFi carrier) own the
  things that must overlap: the blinking LED, flash writes, the WiFi
  stack. They cross back into the loop through channels and signals.

## Highlights

- **Board = data, node = behaviour.** `Board::wire` is pure pin
  bookkeeping; every behaviour lives once in `src/node` and is shared
  by every board.
- **One transport, three media.** LoRa is always interface 0; the
  `wifi` feature adds UDP (1) and TCP (2). Transport mode is on, so a
  WiFi node is a live LoRa↔TCP/UDP bridge for the mesh.
- **The app is a plug.** `node::DefaultApp` resolves from a feature;
  the loop, the node and the carrier are untouched by which app runs.
- **LXMF is the remote control.** Apps talk through the shared
  `responder` — a command-in/reply-out function, with encryption,
  paths and retries below the seam.
- **Failure is logged, never fatal.** A dead LoRa radio, a missing
  sensor, a failed WiFi join: the board keeps running everything else
  and says why (`state=down reason=…`).
- **Everything remote-set survives reset.** Identity, PID tune, blink
  frequency and WiFi settings each live in their own flash sector.

## Execution model

```
        boot                              runtime
┌───────────────────┐         ┌──────────────────────────────┐
│  node::run::<B>   │         │   mainloop::run (async fn)   │
│  1. init + heap   │         │   loop { poll … sleep }      │
│  2. wire          │         └──────────────┬───────────────┘
│  3. esp_rtos      │                        │ spawns
│  4. boot::early   │                        ▼
│  5. carriers      │         ┌──────────────────────────────┐
│  6. sensors       │         │  Embassy tasks               │
│  7. identity      │         │  app: led_task, persist_task │
│  8. node build    │         │  wifi: connection, net, udp, │
│  9. interfaces    │         │        tcp                   │
│ 10. app::start    │         └──────────────────────────────┘
└───────────────────┘
```

Memory: without `wifi`, an `embedded-alloc` heap of 128 KiB
(`HEAP_SIZE`) serves `alloc`. With `wifi`, `esp-alloc` is the global
allocator instead — one 192 KiB heap serves both the radio driver's
internal mallocs and Rust's `alloc`, and the separate `HEAP_MEM` static
is not pinned.

## Boot order

`node::run::<B>(spawner)` wires the system end to end:

```
B::wire(p) ──► esp_rtos::start ──► boot::early ──► carriers::start ──► peripherals::start
     │              (tick)         (log+LED)       (LoRa RX)          (I2C probes)
     ▼
TrngSource ──► reticulum::identity ──► build(NodeCore) ──► register LoRa (iface 0)
     │            (flash or new)         (transport on)         MTU 508
     ▼
wifi::start ──► register wifi (1, 2) ──► storage::share ──► DefaultApp::start
 (settings from                 (flash mutex)            (dest + tasks)
  flash or env)
     ▼
mainloop::announce ──► mainloop::run  (forever)
```

1. `crate::init()` — CPU clock max, heap allocator.
2. `B::wire(p)` — split `Peripherals` into `System` (shared), `led`,
   `lora`, `sensors`, `app` wiring. Pure pin bookkeeping.
3. `esp_rtos::start(timg0, sw_int0)` — scheduler tick + time driver.
4. `boot::early` — USB debug log, banner, status LED parked dark.
5. `carriers::start` — SX1262 init → configure → continuous RX.
   Failure is logged (`state=down`), not fatal.
6. `peripherals::start` — probe the I2C add-ons (BMV080, BME690).
7. `TrngSource` held for the node's lifetime, then
   `reticulum::identity` — load from flash or generate-and-save.
8. `reticulum::build` — the `NodeCore` transport, transport enabled.
9. `reticulum::register_interface` — LoRa = `InterfaceId(0)`, MTU 508.
10. `wifi::start` + `wifi::register_interfaces` (`wifi` feature):
    `wifi_udp` = id 1, `wifi_tcp` = id 2. Settings come from the flash
    slot when one is stored, else the compiled env defaults — and can
    be changed on the debug port at runtime.
11. `storage::share` — flash handle into a static mutex.
12. `DefaultApp::start(ctx, app_wiring)` — the app registers its
    destination and spawns its tasks.
13. `mainloop::announce`, then `mainloop::run` forever.

## Interfaces

| InterfaceId | medium | framing | MTU |
|---|---|---|---|
| 0 | SX1262 LoRa | RNode split frames (≤ 254 B) | 508 |
| 1 | WiFi UDP (feature) | HDLC per datagram | 1400 |
| 2 | WiFi TCP client (feature) | HDLC stream | 1500 |

```
                     ┌──────────────────────────┐
                     │      NodeCore (mesh)     │
                     │  transport mode: on      │
                     └───┬──────────────┬───────┘
                         │              │
       LoRa iface 0      │   WiFi 1, 2  │
   ┌─────────────────────▼──┐   ┌───────▼──────────┐
   │ SX1262, 869.463 MHz    │   │ UDP · TCP over   │
   │ SF8/BW125/CR4/5, 22dBm │   │ station radio    │
   └────────────────────────┘   └──────────────────┘
        peers on the air            LAN / Internet hubs
```

Transport is enabled, so with `wifi` the node is a live LoRa↔TCP/UDP
bridge for the rest of the mesh.

## Where the packets flow

Ingress — two media, one node:

```
LoRa: radio.poll_rx ──► SplitReassembler ──► node.handle_packet(0, …)
WiFi: udp/tcp tasks ──► HDLC Deframer ──► INBOUND channel ──► node.handle_packet(1|2, …)
                                     every result ──► handle(): app.on_events → dispatch
```

Egress — one node, two media:

```
TickOutput ──► Ctx::dispatch
   ├── wifi::egress(out)   — SendPacket/Broadcast for ifaces 1,2 → TX channels
   └── carrier.dispatch    — iface 0 only → build_lora_frames → radio.transmit
```

## Feature flags

| feature | selects |
|---|---|
| *(none)* | `pid_app` — the PID valve regulator |
| `lxmf-echo` | `echo` — LXMF echo inbox |
| `lxmf-blink` | `blink` — LED blinker (owns the status LED) |
| `wifi` | the WiFi carrier (UDP + TCP interfaces) |
| `display` | the V4's SSD1315 OLED status screen (`src/display.rs`, shared `leviculum-screen` painter) |
| `bmv080-sdk`, `bme690` | the I2C sensor drivers |

`lxmf-echo` and `lxmf-blink` are mutually exclusive (compile_error).
The app pick is one type alias in `src/node/mod.rs`:

```
                    ┌───────────────────────────┐
                    │  DefaultApp (by feature)  │
                    └──────┬──────────┬─────────┘
                           │          │
         ┌─────────────────┘          └──────────────┐
         ▼                                          ▼
   feature lxmf-echo                         feature lxmf-blink
         ▼                                          ▼
   app::echo::EchoApp                       app::blink::BlinkApp

         (no feature) ──► app::pid_app::PidApp
```

## Flash map

One 4 KiB sector per durable thing, at 7 MiB (inside any 8 MB module):

```
0x700000  PID slot:   PIDC frame + report target
0x701000  ID slot:    the node's private key (identity)
0x702000  blink slot: the blink frequency (hundredths of Hz)
0x703000  wifi slot:  ssid/psk/udp/tcp settings (wifi feature)
```

No wear levelling: config writes are user events. The WiFi slot makes
`WIFI SSID …` + `WIFI SAVE` survive reset — no rebuild needed.

## Building and flashing

See `docs/build-and-flash.md`: the `esp` toolchain and
`xtensa-esp32s3-none-elf` target, `cargo build --release`, `espflash
save-image`/`flash`, and `just build-esp32`.

## Module map (`src/node/`)

See `docs/node/` for one file per module:

- `board.md` — the `Board` trait and the wiring types
- `boot.md` — log, LED, banner, halt behaviour
- `carriers.md` — the LoRa carrier
- `reticulum.md` — identity, `NodeCore`, interface registration
- `storage.md` — the shared flash handle
- `app.md` — the `App`/`Ctx` seam and the three applications
- `responder.md` — the shared LXMF inbox
- `mainloop.md` — the cooperative event loop
- `peripherals.md` — the I2C add-on sensors
- `wifi.md` — the WiFi carrier, runtime settings, tasks
- `display.md` — the LNode's status screen (Qwiic OLED add-on)

Around it: `docs/bin.md` (the binaries: `xiao_s3`, `xiao_s3_auto`,
`xiao_s3_echo`, `heltec_v4`) and `docs/boards.md` (the pin-map
modules: `xiao_s3`, `heltec_v4`).