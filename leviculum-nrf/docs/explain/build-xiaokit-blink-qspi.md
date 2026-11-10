# Building `xiaokit_blink_qspi` — the XIAO nRF52840 blink firmware

`leviculum-nrf/target/xiaokit_blink_qspi.{bin,uf2}` is not a separate
source target: it is the **`xiaokit` binary** built with the
`lxmf-blink` app feature — the same pattern as the ESP side's
`xiao_s3 --features lxmf-blink`. The `qspi` in the name is historical;
the image is the LXMF blink app on the XIAO nRF52840 kit.

## Build

```sh
source ~/export-esp.sh 2>/dev/null   # not needed for nRF — see below
cd leviculum-nrf
cargo build --release --bin xiaokit --features bsp-xiaokit,lxmf-blink
```

Unlike the ESP crate, `leviculum-nrf` cross-compiles with the **stock
rustc** (thumbv7em-none-eabihf) — no esp-rs fork. What it needs on
PATH is the ARM toolchain: `arm-none-eabi-gcc` (linker via flip-link),
installed by `just`/`scripts/install-ci.sh`.

The ELF lands at
`target/thumbv7em-none-eabihf/release/xiaokit`.

## Packaging

```sh
# flat binary for the UF2 container
arm-none-eabi-objcopy -O binary \
    target/thumbv7em-none-eabihf/release/xiaokit \
    target/xiaokit_blink_qspi.bin
# UF2 (the board's bootloader drive takes UF2, family 0xADA52840)
rustc tools/bin2uf2.rs -o target/bin2uf2 --edition 2021
target/bin2uf2 < target/xiaokit_blink_qspi.bin \
    > target/xiaokit_blink_qspi.uf2
```

(`just flash-xiaokit` does this end-to-end via `cargo run` — the
`.cargo/config.toml` runner is `tools/uf2-runner.sh`, which builds,
converts and copies onto the bootloader drive.)

## LoRa profile

The shared `RadioConfig::eu_medium()` (src/lora.rs) — the fleet-wide
settings every leviculum device runs:

| setting | value |
|---|---|
| frequency | 869463000 Hz |
| bandwidth | 125000 Hz |
| spreading factor | 8 |
| coding rate | 4/5 |
| tx power | 22 dBm |
| preamble | 18 symbols (derived) |

The Blink app on the XIAO kit uses this profile verbatim.

## Flash

Double-tap RESET to enter the UF2 bootloader, then either copy
`target/xiaokit_blink_qspi.uf2` onto the `XIAO-S3` drive or run
`just flash-xiaokit` with `LEVICULUM_FLASH_ONLY=<drive>`.