//! Persistence: the internal-flash stores that outlive a reboot.
//!
//! Everything here must come after the SoftDevice is enabled: writing
//! internal flash with it running is only legal through its own
//! `sd_flash_*` syscalls.

use embassy_executor::Spawner;

use crate::flash::SharedFlash;
use crate::node::board::Board;

/// Spawn the radio-config, telemetry-target and message-store tasks over
/// the one SoftDevice flash handle.
pub async fn start<B: Board>(
    spawner: &Spawner,
    sd: &'static nrf_softdevice::Softdevice,
) -> &'static SharedFlash {
    // Radio-config persistence. Must come after `ble::init`: writing internal
    // flash with the SoftDevice enabled is only legal through its own
    // `sd_flash_*` syscalls, which need the enabled SoftDevice.
    let shared_flash = crate::flash::shared_flash(sd);
    crate::radio_store::spawn_store_task(
        spawner,
        shared_flash,
        B::CONFIG.radio_config_flash_page,
    );
    // Telemetry-target persistence (#236): its own page (0xEA000, reserved
    // in memory.x beside identity and radio config, above the linker's
    // FLASH region and at the bootloader's USER_FLASH_END so it survives a
    // UF2 update), borrowing the same one-and-only SoftDevice flash handle.
    crate::telemetry::spawn_store_task(
        spawner,
        shared_flash,
        B::CONFIG.telemetry_flash_page,
    );

    // The message store (#384): mounts the record log on the region
    // `memory.x` reserves behind the image, formats it once if it is not ours,
    // and then does nothing until something asks it to append — nothing does
    // yet except the `--store-storm` bench instrument. Borrows the same
    // one-and-only SoftDevice flash handle as the two stores above, per
    // operation rather than per append (see `record_store` module docs).
    crate::record_store::spawn_store_task(spawner, shared_flash);

    // Codeberg #384 size harness, behind a throwaway non-default feature:
    // appends, iterates and removes once so the linker keeps the candidate
    // store's code and `arm-none-eabi-size` can weigh it. No shipped build
    // enables this.
    #[cfg(any(feature = "store-spike-record-log", feature = "store-spike-sequential"))]
    crate::store_spike::exercise(shared_flash).await;

    shared_flash
}
