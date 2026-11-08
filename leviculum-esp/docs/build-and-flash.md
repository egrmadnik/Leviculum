# Building and flashing `leviculum-esp`

How the firmware goes from source to a running LNode on an ESP32-S3
board (XIAO ESP32-S3 + Wio-SX1262, Heltec V4).

## The pipeline at a glance

```
 source (src/bin, src/node, build.rs)
    │
    ▼
 esp toolchain (rustc fork, channel "esp")
    │  rust-toolchain.toml selects it on `cd leviculum-esp`
    │  .cargo/config.toml sets the xtensa target + link flags
    │  build.rs embeds git sha + links the sensor SDKs
    ▼
 cargo build --release ──► target/xtensa-esp32s3-none-elf/release/<bin>
    │                       (ELF: loadable sections + DWARF)
    ▼
 espflash save-image ──► <bin>.bin   (ESP-IDF image the ROM accepts)
    │
    ▼
 espflash flash ──► board (hold BOOT + tap RESET if needed)
    │
    ▼
 [FW_BUILD] git_sha=… dirty=…  ← the boot log proves which commit ran
```

## Toolchain — why `esp`, not stable

The target is `xtensa-esp32s3-none-elf`. Stock rustc does not emit that
triple — there is no `rustup target add` for it. The compiler is the
esp-rs fork, installed by **espup** as the `esp` toolchain (pinned in
`scripts/install-ci.sh` to Xtensa Rust 1.97.0.0, the same generation as
the 1.97.1 host pin — codegen decides frame sizes, so one generation
builds both worlds).

Three pieces of configuration make `cargo build` inside
`leviculum-esp/` just work:

| file | what it fixes |
|---|---|
| `rust-toolchain.toml` | `channel = "esp"` — selects the esp-rs fork on entering the directory |
| `.cargo/config.toml` | `target = "xtensa-esp32s3-none-elf"` (overrides the repo-root musl triple), `build-std = ["core", "alloc"]` (no pre-built core ships for this triple), and the link flags: `-nostartfiles` + esp-hal's `linkall.x`, plus `--no-warn-rwx-segments` (the ESP-IDF IRAM/IROM layout produces an RWX LOAD segment — a platform property, not a bug) |
| `~/export-esp.sh` | what espup writes: `xtensa-esp-elf-gcc` (the linker) and `LIBCLANG_PATH` on PATH. **Source it before every build.** |

The crate is its own cargo workspace — entering `leviculum-esp/` is
what selects the toolchain, so there is no `+esp` on the command line.

## Build

```sh
source ~/export-esp.sh
cd leviculum-esp
cargo build --release --bin xiao_s3                # PID app (default)
cargo build --release --bin xiao_s3 --features lxmf-blink
cargo build --release --bin heltec_v4
```

`--release` is the profile that matters: `opt-level = "z"`, `lto`,
`codegen-units = 1`. `strip = "none"` and `debug = true` keep DWARF in
the ELF for `addr2line` on a crash PC — it does not bloat the image,
because only loadable sections are packaged.

The ELF lands at
`target/xtensa-esp32s3-none-elf/release/<bin>`.

### Features

| feature | what it adds |
|---|---|
| *(none)* | the PID valve regulator (`app::pid_app`) |
| `lxmf-echo` / `lxmf-blink` | swap the app for the LXMF echo inbox / LED blinker — mutually exclusive |
| `wifi` | the WiFi carrier: `esp-radio` + `embassy-net`; needs `LEVICULUM_WIFI_SSID` (and `_PSK`, `LEVICULUM_TCP_TARGET`, …) at build time — see `docs/node/wifi.md` |
| `bme690` | compiles Bosch's BSD-3 `bme69x.c` + `csrc` glue with `xtensa-esp32s3-elf-gcc` (build.rs; `-mlongcalls` is required — Xtensa's short `call8` can't cross the IRAM/IROM split) |
| `bmv080-sdk` | links Bosch's licensed `lib_bmv080.a`/`lib_postProcessor.a`; locate it via `BMV080_SDK` or the `docs/sensors` drop |

`build.rs` also embeds the short git SHA + dirty flag into the firmware
(`FW_BUILD_STAMP`) — the `[FW_BUILD]` banner line proves which commit
flashed the board.

### The app descriptor

Every bin calls `esp_bootloader_esp_idf::esp_app_desc!()` — a static in
a named link section that becomes the ESP-IDF application descriptor.
Without it `espflash save-image` produces an image the ROM bootloader
declines.

## Package

`cargo build` produces only the ELF. The ROM wants the ESP-IDF image
format:

```sh
espflash save-image --chip esp32s3 --flash-size 16mb \
    target/xtensa-esp32s3-none-elf/release/xiao_s3 \
    target/xiao_s3.bin
```

(`--flash-size 8mb` for the XIAO; `16mb` matches the Justfile for
Heltec.) **`save-image` does not overwrite an existing file** — delete
a stale `.bin` first or the old image is silently kept.

## Flash

```sh
espflash flash --chip esp32s3 --port /dev/ttyACM0 target/xiao_s3.bin
```

If the board does not enter the ROM bootloader on its own, hold **BOOT**
and tap **RESET**. The debug log (`[BOARD]`, `[FW_BUILD]`, `[LORA]`,
…) comes back over the same USB Serial/JTAG port the flasher used.

## One command — `just build-esp32`

From the repo root, `just build-esp32` runs the whole gate: sources
`export-esp.sh`, then `cargo fmt --check`, `clippy --release -D
warnings`, `cargo doc -D warnings` (repeated here because the root
doc-gate never reaches this excluded workspace), `cargo build
--release`, and `espflash save-image` for `heltec_v4` and `xiao_s3`.

## WiFi build example

```sh
source ~/export-esp.sh
cd leviculum-esp
LEVICULUM_WIFI_SSID=lab LEVICULUM_WIFI_PSK=secret \
LEVICULUM_TCP_TARGET=192.168.1.10:4242 \
cargo build --release --bin xiao_s3 --features wifi
```

With no `LEVICULUM_WIFI_SSID` compiled in, boot logs
`[WIFI] state=dark ssid=none cfg=default` — and the radio can still be
brought up at runtime: `WIFI SSID <name>` + `WIFI SAVE` on the debug
port, no rebuild.

## Key points

- **Source it first.** `~/export-esp.sh` is what puts the Xtensa GCC
  linker on PATH; without it the link step fails.
- **`--release` is the only profile that fits** (`opt-level = "z"`,
  LTO); DWARF is kept in the ELF for `addr2line` but never lands in
  the image.
- **Two steps, two formats.** `cargo build` gives the ELF; only
  `espflash save-image` produces the bootable image the ROM accepts —
  and it refuses to overwrite a stale `.bin`.
- **The boot log is the receipt.** `[FW_BUILD] git_sha=… dirty=…`
  proves which commit's bytes are on the board.
