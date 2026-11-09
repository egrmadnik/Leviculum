# How the firmware builds — and where `lxmf-blink` is

`lxmf-blink` is not a file or a binary — it is a **Cargo feature** (a
build-time switch). That is why it cannot be found by name in the tree:
the code it selects is `src/node/app/blink.rs`.

## Where `lxmf-blink` lives

| place | role |
|---|---|
| `leviculum-esp/Cargo.toml` | `[features] lxmf-blink = []` — the feature is *declared* here |
| `src/node/app/mod.rs` | `#[cfg(feature = "lxmf-blink")] pub mod blink;` — the module is compiled only when the feature is on |
| `src/node/mod.rs` | `#[cfg(feature = "lxmf-blink")] pub type DefaultApp = app::blink::BlinkApp;` — the feature *selects the app* |
| `src/node/app/blink.rs` | the app itself (LED task, persist task, `blink <Hz>` reply) |

Same pattern for `lxmf-echo` (`app/echo.rs`); enabling both is a
`compile_error!`.

## How the firmware builds

```
source ~/export-esp.sh          # puts the xtensa linker on PATH
cd leviculum-esp                # selects the "esp" rustc toolchain
cargo build --release --features lxmf-blink,display --bin xiao_s3
        │                          │                    └─ the board binary
        │                          └─ app + display features
        └─ release profile (size-optimized, LTO)
   → target/xtensa-esp32s3-none-elf/release/xiao_s3   (the ELF)
   → espflash flash --port /dev/ttyACM2 <that ELF>    (boot the image)
```

Each `--features` you pass changes what the same binary contains:

| features | app on the board |
|---|---|
| *(none)* | PID regulator (`pid_app`) |
| `lxmf-blink` | the blink app (`blink <Hz>` over LXMF) |
| `lxmf-echo` | the echo inbox |
| `display` | OLED support (either app) |
| `wifi` | adds UDP/TCP interfaces (any app) |

That is why the currently flashed image is `--features lxmf-blink,display`:
the board's `[BLINK] start hz=1.00` boot line proves the blink app is in
it — there is no file named `lxmf-blink` anywhere; it is the feature
that pulls `app/blink.rs` in.

See `docs/build-and-flash.md` for the full toolchain (why `esp`, not
stable rustc) and `docs/bin.md` for the app/feature matrix across all
binaries.