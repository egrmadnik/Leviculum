# Building `xiao_s3_blink.bin`

`leviculum-esp/target/xiao_s3_blink.bin` is the XIAO ESP32-S3 + Wio-SX1262
firmware with the **Blink app** — the LED blinker a remote node drives over
LXMF (see [The Blink app](blink-app.md)). It is not a separate binary
target: it is the `xiao_s3` bin built with the `lxmf-blink` cargo feature
and saved under a different name.

## Steps

```sh
cd leviculum-esp
. ~/export-esp.sh                     # xtensa-esp-elf-gcc on PATH (the linker)
cargo build --release --bin xiao_s3 --features lxmf-blink
espflash save-image --chip esp32s3 --flash-size 8mb \
    target/xtensa-esp32s3-none-elf/release/xiao_s3 \
    target/xiao_s3_blink.bin
```

`espflash save-image` **does not overwrite an existing file**, so delete a
stale `target/xiao_s3_blink.bin` first or the old image is silently kept.

## What each piece contributes

| Piece | Where | Role |
| --- | --- | --- |
| Toolchain | `leviculum-esp/rust-toolchain.toml` | Selects the `esp` toolchain — the esp-rs rustc fork. The `xtensa-esp32s3-none-elf` triple does not exist in stock rustc. |
| Target + linker flags | `leviculum-esp/.cargo/config.toml` | Sets `target = "xtensa-esp32s3-none-elf"` (overriding the repo-root musl triple), passes `-nostartfiles` and esp-hal's `linkall.x` to the linker, and builds `core`/`alloc` from source (`build-std`). |
| App selection | `--features lxmf-blink` | `src/node/mod.rs` picks `blink::BlinkApp` as `DefaultApp` (default is the PID regulator). `lxmf-blink` and `lxmf-echo` are alternatives; enabling both is a `compile_error!`. |
| Binary | `src/bin/xiao_s3.rs` (`[[bin]] xiao_s3`) | Board definition for the XIAO + Wio-SX1262 pinout plus `node::run`. The ELF lands at `target/xtensa-esp32s3-none-elf/release/xiao_s3`. |
| Image packaging | `espflash save-image --chip esp32s3 --flash-size 8mb` | Converts the ELF's loadable sections into the ESP-IDF image format the ROM bootloader accepts. `esp-bootloader-esp-idf` (the `esp_app_desc!` static in the bin) supplies the app descriptor; without it the ROM refuses the image. |

## Notes

- The Blink app code is `leviculum-esp/src/node/app/blink.rs`; its persisted
  frequency record (`LBLK`) lives in flash sector `0x702000`
  (`leviculum-esp/src/store.rs`).
- A UF2 (`target/xiao_s3_blink.uf2`, family `0xC47E5767`, base `0x10000`)
  exists only as a file format: the stock XIAO ESP32-S3 has no UF2
  bootloader. Real flashing is `espflash flash --chip esp32s3 --port
  /dev/ttyACM0 target/xiao_s3_blink.bin` (hold BOOT, tap RESET if it does
  not enter the bootloader).
- The release profile (`opt-level = "z"`, LTO, `strip = "none"`,
  `debug = true`) keeps DWARF in the ELF for `addr2line`; only loadable
  sections reach the `.bin`.
