# Next session: LNode modular refactor, other boards

State at the end of the XiaoKit session (2026-11-06). Written as a handoff;
nothing here has been run on CI.

## Done on the XiaoKit only

- The shared code lives in `leviculum-nrf/src/node/` (`board`, `boot`,
  `reticulum`, `carriers`, `storage`, `peripherals`, `readings`, `mainloop`,
  `tasks`). `bin/xiaokit.rs` is a `Board` implementation plus one `run` call
  (198 lines, was 1,390) and was measured slightly smaller than the original.
- Apps on top of it, chosen by cargo feature: `lxmf-echo` and `lxmf-blink`
  (alternatives; both together is a compile error). Shared LXMF part:
  `node/responder.rs`. The Blink setting is stored as record `LBLK` at
  `+0x600` of the telemetry flash page.
- XiaoKit now declares and asks for its 2 MB P25Q16H at boot
  (`boards/xiaokit.rs`, `bin/xiaokit.rs`).
- `t114.rs`, `solarnode.rs` and `rak4631.rs` are **untouched** and still the
  old monolithic binaries.

## To do

1. **Port the other boards** to `Board` implementations: `t114`, `solarnode`,
   `rak4631`. Read the originals first (the hooks `power_up`, `qspi`,
   `spawn_display`, `boot_indicator`, `after_boot_banner` and the consts
   exist for exactly their differences). Compare each ported image's size
   with its original, as was done for the XiaoKit.
2. **Fix the board-pins gate** (`scripts/check-nrf-board-pins.sh`,
   `leviculum-nrf/reference-pins.toml`). It has not been run since the
   refactor, and it will not pass as it stands:
   - It needs Python 3.11+ (`tomllib`). The default `python3` here is 3.10;
     `python3.12` is installed.
   - Claim 2 looks for `qspi::identify_and_mount_bounded(` and `lora::init(`
     in the board's **bin** with `p.P0_xx` arguments. After the refactor the
     LoRa call is in `node/carriers.rs` with pins from `LoraWiring`, and the
     XiaoKit's QSPI call takes its pins from a `QspiPins` struct. The gate
     must read where the pins are now named (the `wire` function), or the
     call sites must be made greppable again.
   - Claim 4 and `TREE_PARTS` say the XiaoKit has **no** QSPI part
     (`"none"`: no `Qspi*` aliases, no identify call). That is no longer
     true: it is `P25Q16H`. Change `TREE_PARTS`, `reference-pins.toml`
     (`qspi_part`, add a `[boards.xiaokit.qspi]` table) and the header
     comments of both files, and add the six `Qspi*` aliases to
     `boards/xiaokit.rs`.
   - Claim 3 cannot derive the XiaoKit's QSPI defines from its own variant
     header, which comments them out. Either point the table at the solar
     node's variant or let an entry say its pin is checked against the
     schematic instead.
3. **Check the flash window.** The XiaoKit Blink image leaves 38,256 B free
   in the 729,088 B application window (`xiaokit_blink_qspi.uf2`). The
   other boards carry more (display, GNSS); run
   `scripts/check-nrf-store-gap.sh` for each.
4. **Docs.** `docs/src/firmware/boards.md` was updated for the XiaoKit's
   QSPI part; it and the `GLOSSARY.md` terms (Board, Reticulum module, LXMF
   service, App task, Blink app, Command, Report) should be rechecked once the
   other boards use `node/`.

## Open, not blocking

- Persistence across a reset of the Blink setting was only seen as
  `[BLINK] saved`; reset the board and look for `[BLINK] start ... src=stored`.
  The `blink 0` (off) case was not sent.
- `[LXMF]` log lines in `responder.rs` say "echo" for every reply (cosmetic,
  left as is on request).
- `0xED000` as a possible extra persistence page on the XIAO was **not**
  verified and is not used (the repo's flash map treats everything above
  `0xEC000` as the T114's Heltec band).
- A XiaoKit app that sends reports on its own needs `Responder::send`; see
  the "Reports nobody asked for" section of
  `docs/src/firmware/automation-app-guide.md`. The sender's hash is also not
  passed to the reply function, which an allow-list needs.

---

# ESP32-S3 (XIAO ESP32-S3 + Wio-SX1262), same session

The nRF XiaoKit work above has a counterpart in `leviculum-esp`. State at the
end of the session:

## Done

- **Modular `node/`.** `leviculum-esp/src/node/` mirrors the nRF layout:
  `board` (the `Board` seam: pins, LoRa profile, LED polarity), `boot` (log,
  LED, banner), `carriers` (SX1262 bring-up, receive, transmit),
  `reticulum` (identity, node), `peripherals` (BMV080/BME690 add-on sensors),
  `storage` (shared flash handle), `app` (`App` trait, `Ctx`), `mainloop`.
  `bin/xiao_s3.rs` is the board definition plus `node::run` (about 120 lines,
  was 1,048). The original monolith is saved at `/tmp/espbase/xiao_s3_orig.rs`
  (not in the repo, will not survive a reboot of the dev machine).
- **Embassy.** The runtime is now `esp-rtos` (Embassy integration) over
  `esp-hal`: `main` is `#[esp_rtos::main] async`, `node::run` and the loop are
  async, an app spawns Embassy tasks. New dependencies in `Cargo.toml`:
  `esp-rtos` 0.4, `embassy-executor` 0.10 (no `arch-*` feature),
  `embassy-time` 0.5, `embassy-sync` 0.8, `static_cell`. All five binaries of
  the crate build.
- **Apps, one per build (cargo feature):** PID regulator (default, unchanged
  behaviour, now ticks under the executor), `lxmf-echo`, `lxmf-blink`. The
  shared LXMF inbox is `node/responder.rs`, same shape as the nRF's; echo and
  blink are thin on top. Blink has an Embassy `led_task` and `persist_task`;
  its record is `LBLK` in a new flash sector at `0x702000`
  (`leviculum-esp/src/store.rs`). The board has one LED (yellow, GPIO21,
  active low), which the Blink app takes over from the status banner
  (`App::OWNS_STATUS_LED`).
- **Receive bug fixed (`radio.rs`).** `buf.len() as u8` turned the 256-byte
  receive buffer into 0, so every frame was read as zero bytes long and the
  ESP never received anything, in any build. The length is clamped before
  narrowing. Because the old code made the downstream receive path dead, image
  sizes jumped: Blink 649,600 B, echo 639,552 B, PID 600,432 B (the old
  monolith, 534,608 B, is not comparable).
- **Hardware test:** the Blink image was flashed (`espflash flash --chip
  esp32s3 --flash-size 8mb`, port `/dev/ttyACM2`) and driven from the host
  through the nRF kit: commands arrive, replies come back, the setting is
  saved (`[BLINK] saved hz=…`), the LED blinks.

## To do

1. **Persistence across a reset** of the Blink setting: reset the board and
   look for `[BLINK] start hz=… src=stored` (only the save was seen). Also
   send `blink 0`.
2. **`heltec_v4`** shares `radio.rs`: it got the receive fix but was never
   run. Its binary still has the old monolithic structure; porting it to
   `node/` is the same job as the other boards.
3. **PID app on hardware.** It builds and boots under the executor, but its
   remote-config path (`PIDC` frame over LXMF, status `PIDS` reports) has not
   been exercised since the receive fix, which makes it the first time it
   *can* run. Test it before anyone relies on it.
4. **`bmv080-sdk` path** of `node/peripherals.rs` was ported but never
   compiled (the licensed Bosch SDK is not in the tree). The `bme690` path
   compiles.
5. **`xiao_s3_auto` and `xiao_s3_echo`** (the `leviculum-automation` bins) are
   untouched and still run the old blocking structure; they build, but their
   receive path was also dead before the fix and has not been retested.
6. **Docs.** `docs/src/firmware/boards.md` and `docs/src/concepts/
   board-support-scope.md` still describe the ESP side as a polling loop
   with no executor, and the flash map note in `store.rs` / the docs lists two
   sectors, now three.
7. **Gates.** `just` recipes and CI for `leviculum-esp` (`cargo fmt --check`,
   `clippy -D warnings`, doc build, `espflash save-image`) were not run;
   only `cargo build --release` was.

## Caveats worth remembering

- **UF2 is a file format here, not a flashing path.** The stock XIAO ESP32-S3
  has no UF2 bootloader (the repo's docs say so); `target/xiao_s3_blink.uf2`
  (family `0xC47E5767`, base `0x10000`) works only if TinyUF2 is installed.
  Real flashing was `espflash`. The `0x10000` base is the default factory-app
  offset and was not checked against a partition table.
- **`espflash save-image` does not overwrite an existing file.** A script that
  hides its output will happily convert a stale `.bin`; delete the target
  first. This produced a wrong UF2 once in the session.
- **Log prefix change:** the identity lines are now `[IDENTITY] ...` (were
  `[PID] identity=...`). Nothing in this repo greps the old form, but an
  external capture script might.
- **ESP boot lines are one-shot.** `[IDENTITY]`, `[LXMF] inbox ready`,
  `[BLINK] start`, `[LORA] state=up` print once at boot; the 5-second banner
  repeats only `[FW_BUILD]` and `[BOARD]`. To get the delivery hash of a
  running board, listen for its announce (display name
  `leviculum-xiao_s3-blink`) on the host, as was done this session.
