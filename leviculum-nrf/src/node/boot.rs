//! Boot: the first thing every LNode does, in the order every board has
//! always done it.
//!
//! Two steps. [`init`] captures the previous boot's breadcrumbs and brings
//! the chip up; [`early`] does everything that has to happen before the
//! SoftDevice is enabled: post-mortems, the persisted media / name /
//! management records, the boot counter, USB and the boot banner.

use embassy_executor::Spawner;
use embassy_nrf::{peripherals, Peri, Peripherals};

use crate::node::board::Board;
use crate::usb::SerialChannels;
use crate::{init_heap, log_critical};

/// What [`early`] hands to the rest of the boot.
pub struct Booted {
    pub media: leviculum_core::envelope::MediaProfileWire,
    pub media_src: crate::media::Source,
    pub mgmt_allow: crate::mgmt::RemoteMgmtDecision,
    pub vbus: &'static crate::SoftwareVbusDetect,
    pub serial: SerialChannels,
}

/// Capture the previous boot's record and initialise the chip.
pub fn init() -> (crate::boot_trace::Captured, Peripherals) {
    // First statement on purpose: reads the PREVIOUS boot's breadcrumbs
    // and RESETREAS (and clears it), then arms this boot's record at
    // enter-main — so even a hang inside `embassy_nrf::init`'s HFXO wait
    // is attributable on the next boot's BOOT_TRACE line.
    let boot = crate::boot_trace::capture();

    let mut config = embassy_nrf::config::Config::default();
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);
    (boot, p)
}

/// Everything before the SoftDevice: see the module docs.
pub fn early<B: Board>(
    spawner: &Spawner,
    boot: &crate::boot_trace::Captured,
    nvmc: &mut Peri<'static, peripherals::NVMC>,
    usbd: Peri<'static, peripherals::USBD>,
) -> Booted {
    init_heap();
    crate::init_tracing();

    // Boot diagnostics from the previous run. Read before the first log
    // call of this boot: `take_persistent_log` must snapshot the PREVIOUS
    // boot's tail before we start writing this boot's into the same ring.
    let hardfault_pm = crate::take_hardfault_postmortem();
    let panic_pm = crate::take_panic_postmortem();
    let persistent_log = crate::log::take_persistent_log();

    // SAFETY: called once before any complex work or concurrent tasks.
    // Under flip-link the painted region is `[_stack_end, SP-1KiB)` at
    // the BOTTOM of RAM — disjoint from `.uninit`, so the post-mortem
    // reads above are no longer order-critical against it.
    unsafe {
        crate::paint_stack();
    }

    crate::set_panic_led(B::PANIC_LED.0, B::PANIC_LED.1, B::PANIC_LED.2);
    // A distinct LED for HardFault — the blue cell of the RGB on P0.06,
    // active LOW like the green — so a bring-up can tell a fault from a
    // panic across the room, before the first serial line exists. Same
    // reasoning as the Pocket V2's (`bin/rak4631.rs`).
    crate::set_hardfault_led(B::HARDFAULT_LED.0, B::HARDFAULT_LED.1, B::HARDFAULT_LED.2);
    crate::set_irq_priorities();
    // This binary wires a telemetry Reporter into its main loop; declared
    // before USB comes up so the serial task can never answer a telemetry
    // target ahead of the declaration (ack honesty, #236).
    crate::telemetry::declare_reporter();
    // Which position sources this board boots with — the second clause of
    // the telemetry send condition, and a question a host may put before
    // the reporter exists, so it is answered before USB comes up. The
    // GNSS bit says a receiver is *configured*, not that it has a fix:
    // reports run `position=0` until the L76K acquires and `position=1
    // possrc=gnss` after (#69, #255). `cfg!` and not a constant, because
    // the spawn below is under the same gate — the two must not be able
    // to disagree.
    crate::telemetry::declare_position_sources(
        /* gnss_available */ cfg!(feature = "gnss"),
        B::CONFIG.telemetry_flash_page,
    );
    // The media profile decides which carriers come up at all, so it is
    // read before USB and before either carrier: before USB so a host
    // frame can never be answered against the default while the flash
    // record is still unread, and before the carriers because the answer
    // is their spawn decision. A memory-mapped flash read, legal this
    // early and before `Softdevice::enable`.
    let (media, media_src) =
        crate::media::load_at_boot(B::CONFIG.telemetry_flash_page);
    // The node's name, read on the same page and for the same reason the
    // profile is: before USB, so a host frame is never answered against
    // the derived default while the record is still unread, and before
    // the first announce and `ble::init`, which both display it.
    crate::name::load_at_boot(B::CONFIG.telemetry_flash_page);
    // Remote management (#235): the identity allow-list this board serves
    // `rnstransport.remote.management` `/status` to, read on the same page
    // and this early because the answer decides how the NODE IS BUILT —
    // the destination is created by the builder below and cannot be added
    // or withdrawn while the node runs. Also before USB, so a host frame
    // is never answered against an unread record.
    let mgmt_allow = crate::mgmt::load_at_boot(B::CONFIG.telemetry_flash_page);
    // How many times this board has restarted, appended to its own flash
    // page. Here because it must be before `Softdevice::enable` — after
    // it, writing internal flash means going through the SD, and a board
    // that boot-loops on a sagging pack rarely gets that far (the whole
    // reasoning is in `boot_count`). The line it produces waits for USB.
    // A XIAO kit runs unattended for months on a pack it charges
    // itself, so it is the board most likely to restart with nobody
    // watching and the least likely to have a cable in it when it does.
    let boot_count = crate::boot_count::record_at_boot(
        embassy_nrf::nvmc::Nvmc::new(nvmc.reborrow()),
        boot,
    );
    crate::boot_trace::phase(crate::boot_trace::Phase::PersistRead);
    let vbus = crate::init_vbus();
    let serial = crate::usb::init(spawner, usbd, vbus, B::CONFIG);
    crate::boot_trace::phase(crate::boot_trace::Phase::UsbUp);

    log_critical!("leviculum {} booting", B::NAME);
    B::after_boot_banner();
    log_critical!("[FW_BUILD] {}", crate::FW_BUILD_STAMP);
    log_critical!("[TIME_SOURCE] source={}", crate::time_source_str());
    crate::log_stack("boot");
    crate::log_panic_count();
    // Boot-loop instrumentation: how far did the previous boot get, and
    // what kind of reset got us here? Both values were captured by
    // `boot_trace::capture` at the top of main (POWER was still ours
    // there; after Softdevice::enable it belongs to the SD).
    crate::boot_trace::log_prev(boot);
    crate::log_reset_reason(boot.reset_reason);
    // ...and how many boots there have been, which is the half of that
    // question retained RAM cannot answer across a power loss (#380).
    crate::boot_count::log_banner(&boot_count);
    crate::log_irq_priorities();

    // Shared boot/query formatter — the same block is retrievable at any
    // later time via the debug-port query (`p` byte, postmortem_query).
    crate::log_postmortems(hardfault_pm.as_ref(), panic_pm.as_ref());
    if let Some(snap) = persistent_log {
        let mut start = 0usize;
        while start < snap.len {
            let end = snap.bytes[start..snap.len]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| start + p + 1)
                .unwrap_or(snap.len);
            if start == 0 && end == snap.len {
                // single-line case
            } else if start == 0 {
                start = end;
                continue;
            }
            let raw = &snap.bytes[start..end];
            let trimmed = core::str::from_utf8(raw)
                .unwrap_or("<non-utf8>")
                .trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                log_critical!("[PERSISTENT_LOG] {}", trimmed);
            }
            start = end;
        }
    }

    Booted {
        media,
        media_src,
        mgmt_allow,
        vbus,
        serial,
    }
}
