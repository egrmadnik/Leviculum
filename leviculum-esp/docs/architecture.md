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

## Execution model

The firmware is `no_std` on `esp-hal` with `esp-rtos` providing the
Embassy executor. One async main loop (`mainloop`) cooperatively polls
everything; work that must overlap (the blinking LED, flash writes, the
WiFi socket tasks) is spawned as Embassy tasks from `App::start` or
`wifi::start`. Nothing in the loop may block.

Memory: without `wifi`, an `embedded-alloc` heap of 128 KiB
(`HEAP_SIZE`) serves `alloc`. With `wifi`, `esp-alloc` is the global
allocator instead — one 192 KiB heap serves both the radio driver's
internal mallocs and Rust's `alloc`, and the separate `HEAP_MEM` static
is not pinned.

## Boot order

`node::run::<B>(spawner)` wires the system end to end:

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
    `wifi_udp` = id 1, `wifi_tcp` = id 2.
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

Transport is enabled, so with `wifi` the node is a live LoRa↔TCP/UDP
bridge for the rest of the mesh.

## Feature flags

| feature | selects |
|---|---|
| *(none)* | `pid_app` — the PID valve regulator |
| `lxmf-echo` | `echo` — LXMF echo inbox |
| `lxmf-blink` | `blink` — LED blinker (owns the status LED) |
| `wifi` | the WiFi carrier (UDP + TCP interfaces) |
| `bmv080-sdk`, `bme690` | the I2C sensor drivers |

`lxmf-echo` and `lxmf-blink` are mutually exclusive (compile_error).

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
- `wifi.md` — the WiFi carrier and its build-time configuration
