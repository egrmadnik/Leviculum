//! The LNode: one firmware, many boards.
//!
//! A board binary is a hardware definition (a [`Board`] implementation)
//! plus a call to [`run`]. Everything an LNode does that does not depend
//! on which unit it runs on is split by function into the modules below,
//! in the order the boot reaches them:
//!
//! | module | what it owns |
//! |---|---|
//! | [`board`] | the seam: pin map, rails, per-board hooks |
//! | [`boot`] | diagnostics, persisted records, USB, boot banner |
//! | [`reticulum`] | identity, the transport node, the three interfaces |
//! | [`carriers`] | the LoRa radio and the BLE peripheral |
//! | [`storage`] | the internal-flash stores |
//! | [`peripherals`] | GNSS and battery |
//! | [`readings`] | telemetry readings from the sensors above |
//! | [`mainloop`] | the cooperative event loop |
//! | [`tasks`] | housekeeping banners |

pub mod board;
pub mod boot;
pub mod carriers;
#[cfg(feature = "lxmf-blink")]
pub mod blink;
#[cfg(feature = "lxmf-echo")]
pub mod echo;
pub mod mainloop;
pub mod peripherals;
pub mod readings;
#[cfg(any(feature = "lxmf-echo", feature = "lxmf-blink"))]
pub mod responder;
pub mod reticulum;
pub mod storage;
pub mod tasks;

#[cfg(all(feature = "lxmf-echo", feature = "lxmf-blink"))]
compile_error!("`lxmf-echo` and `lxmf-blink` are alternative apps: enable one");

use embassy_executor::Spawner;

pub use board::{Board, Wiring};

use crate::info;

/// Boot an LNode on board `B` and run it forever.
pub async fn run<B: Board>(spawner: Spawner) -> ! {
    let (boot, p) = boot::init();
    let Wiring {
        mut board,
        mut common,
        lora,
        #[cfg(feature = "gnss")]
        gnss,
        #[cfg(feature = "battery")]
        battery,
    } = B::wire(p);

    let booted = boot::early::<B>(&spawner, &boot, &mut common.nvmc, common.usbd);

    board.power_up();

    let mut node =
        reticulum::build::<B>(&spawner, common.nvmc, &booted.mgmt_allow, booted.media_src);
    reticulum::register_interfaces(&mut node);

    board.qspi().await;

    let lora_channels = carriers::start_lora::<B>(&spawner, common.spi2, lora, &booted.media).await;

    let identity_hash = *node.identity().hash();
    let (sd, ble_channels) = carriers::start_ble(
        &spawner,
        common.ble,
        &booted.media,
        booted.media_src,
        identity_hash,
        booted.vbus,
    );

    let _shared_flash = storage::start::<B>(&spawner, sd).await;

    board.spawn_display(&spawner, identity_hash);

    peripherals::start::<B>(
        &spawner,
        #[cfg(feature = "gnss")]
        gnss,
        #[cfg(feature = "battery")]
        battery,
    );

    let (hu, hf) = crate::heap_stats();
    info!("heap u={} f={}", hu, hf);
    crate::log_stack("post-init");

    board.boot_indicator(&spawner);

    // The Blink app's LED task, when the board has a blink LED.
    #[cfg(feature = "lxmf-blink")]
    if let Some(led) = board.blink_led() {
        blink::start(&spawner, led, B::CONFIG.telemetry_flash_page);
    }

    mainloop::run::<B>(
        &spawner,
        node,
        booted.serial,
        lora_channels,
        ble_channels,
        sd,
    )
    .await
}
