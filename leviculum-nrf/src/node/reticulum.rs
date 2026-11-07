//! The Reticulum module of an LNode: identity, the transport node and the
//! three interfaces it routes between.
//!
//! Interface 0 is the USB CDC serial line to a host, interface 1 the LoRa
//! radio, interface 2 the BLE peripheral.

use alloc::boxed::Box;
use embassy_executor::Spawner;

use leviculum_core::embedded_storage::EmbeddedStorage;
use leviculum_core::node::{NodeCore, NodeCoreBuilder};

use crate::clock::EmbassyClock;
use crate::node::board::Board;
use crate::node::tasks;
use crate::{info, log_critical};

/// The transport node as every LNode builds it.
pub type Node = NodeCore<crate::rng::RawHwRng, EmbassyClock, EmbeddedStorage>;

/// Load or create the identity, build the transport node and start the
/// housekeeping tasks that report on it.
pub fn build<B: Board>(
    spawner: &Spawner,
    nvmc: embassy_nrf::Peri<'static, embassy_nrf::peripherals::NVMC>,
    mgmt_allow: &crate::mgmt::RemoteMgmtDecision,
    media_src: crate::media::Source,
) -> Box<Node> {
    let rng = crate::rng::RawHwRng::new();

    // Load or generate persistent identity from internal flash
    // `nvmc` is consumed here: the identity store owns the flash controller
    // until the SoftDevice takes flash writes over.
    let mut id_store = crate::flash::NvmcIdentityStore::new(
        embassy_nrf::nvmc::Nvmc::new(nvmc),
        B::CONFIG.identity_flash_page,
    );

    // #388 pass 3: the heap budget's `links=` term, derived from the
    // arithmetic in `heap_census::max_endpoint_links` and ENFORCED as the
    // core's link cap — the budget and the cap are the same constant.
    const MAX_ENDPOINT_LINKS: usize =
        crate::heap_census::max_endpoint_links(core::mem::size_of::<
            leviculum_core::node::NodeCore<
                crate::rng::RawHwRng,
                EmbassyClock,
                EmbeddedStorage,
            >,
        >());
    const _: () = assert!(
        MAX_ENDPOINT_LINKS >= crate::ble::MAX_LINKS,
        "HEAP_BUDGET affords fewer endpoint links than claimable BLE sessions (#388)"
    );

    let mut builder = NodeCoreBuilder::new()
        .enable_transport(true)
        .max_links(Some(MAX_ENDPOINT_LINKS))
        .max_incoming_resource_size(crate::MAX_INCOMING_RESOURCE_BYTES)
        // #402: announces held back by the LoRa announce cap wait here, and
        // on a board the queue is a heap term, not a formality. One entry is
        // its raw announce (~183 B) plus its Vec header, destination, hops
        // and timestamp, about 220 B, so eight entries are ~1.8 KiB of the
        // 45-50 KiB idle heap — against 16384 entries (the host default, and
        // Python's `MAX_QUEUED_ANNOUNCES`), which is no bound at all here.
        // Eight is also a time bound: at SF10 the 2 % share holds an announce
        // ~92 s, so a full queue is ~12 minutes deep, and past that an
        // announce is better re-learned from its source's next one than
        // replayed stale. A full queue drops the ARRIVING announce, the
        // newest one; the queue keeps what it has and drains fewest-hops-
        // first (`Transport::drain_announce_queues`).
        .max_queued_announces(8)
        .max_random_blobs(8)
        .respond_to_probes(true);

    // Only a stored, NON-EMPTY list registers anything. That rule is
    // `leviculum_core::mgmt_allow_store::remote_mgmt_decision`, and it is
    // deliberately not the daemon's: `lnsd` and Python's `rnsd` register
    // the handler with an empty list and consult it per request
    // (`leviculum-std/src/config.rs:88-96`), which is safe on a machine
    // with an operator and a login, and is not safe on a board left on a
    // mast — an unattended node announcing a management destination with
    // nobody on the list is advertising a door (#235).
    if let crate::mgmt::RemoteMgmtDecision::Enabled(allowed) = &mgmt_allow {
        builder = builder.remote_management(true, allowed.clone());
    }

    let identity_loaded = {
        use leviculum_core::identity_store::IdentityStore;
        // Critical for the same reason as the `[IDENTITY]` banner below
        // (#234): this says whether the hash that banner prints was the
        // board's own or made up this boot, and the recovery doc sends an
        // operator to the debug port to read exactly that after a flash.
        if let Ok(Some(identity)) = id_store.load() {
            log_critical!("Identity loaded from flash");
            builder = builder.identity(identity);
            true
        } else {
            log_critical!("No identity in flash, generating new");
            false
        }
    };

    let node = builder.build_boxed(rng, EmbassyClock, EmbeddedStorage::new());
    // #388: state the heap budget and refuse a configuration that
    // cannot fit — at boot, where the panic names the arithmetic. The
    // compile-time twin below fails the build before it can fail a
    // board.
    crate::heap_census::log_budget_and_assert(core::mem::size_of_val(&*node));
    const _: () = assert!(
        crate::heap_census::budget_total(core::mem::size_of::<
            leviculum_core::node::NodeCore<
                crate::rng::RawHwRng,
                EmbassyClock,
                EmbeddedStorage,
            >,
        >()) <= crate::HEAP_SIZE,
        "HEAP_BUDGET total exceeds the heap (#388): shrink a term"
    );

    // What the builder actually did with the allow-list, on the critical
    // log path: `[MGMT] enabled allowed=N`, or `disabled (no allow-list)`
    // for the state a board ships in. Read `remote_mgmt_dest_hash` rather
    // than assuming, so a boot that could not derive the destination says
    // so instead of promising a service nothing serves.
    let mgmt_registered = node.remote_mgmt_dest_hash().is_some();
    crate::mgmt::note_registered(mgmt_registered);
    crate::mgmt::log_banner(&mgmt_allow, mgmt_registered);

    let initial_path_len = node.path_count();
    info!("[BOOT] path_table_initial_len={}", initial_path_len);
    spawner.must_spawn(tasks::boot_log_repeater(initial_path_len));
    spawner.must_spawn(tasks::fw_build_banner(media_src));
    spawner.must_spawn(crate::heap_watermark_task());
    if B::DIAG_MEM_LOG {
        spawner.must_spawn(tasks::diag_mem_log());
    } else {
        spawner.must_spawn(crate::stack_watermark_task());
    }

    if !identity_loaded {
        use leviculum_core::identity_store::IdentityStore;
        let _ = id_store.save(node.identity());
        info!("Identity saved to flash");
    }

    node
}

/// Name the three interfaces and set the mode, MTU and path-discovery
/// behaviour every LNode gives them, then log that the node started.
pub fn register_interfaces(node: &mut Node) {
    // Register all three interfaces
    node.set_interface_name(0, alloc::string::String::from("serial_usb"));
    node.set_interface_hw_mtu(0, 564);
    // Codeberg #117: the client-facing serial interface must be Gateway so the
    // node discovers unknown paths on behalf of the host.
    node.set_interface_mode(0, leviculum_core::InterfaceMode::Gateway);
    node.set_interface_name(1, alloc::string::String::from("lora_sx1262"));
    // Codeberg #390: 508, not 255. Three places already agree on 508 for a
    // LoRa carrier -- the reference (`RNS/Interfaces/RNodeInterface.py`,
    // `HW_MTU = 508`), our own RNode constant (`leviculum-core/src/rnode.rs`,
    // `HW_MTU: usize = 508`), and the splitter that actually puts the bytes on
    // the air (`build_lora_frames`, two frames of at most
    // `MAX_SINGLE_PAYLOAD = 254`). The radio was never the limit; 255 was.
    //
    // What the old 255 cost: an incoming link over this interface clamped to
    // 255 on the responder (`Link::new_incoming`, `path_mtu.min(hw_mtu)`)
    // while the initiator floored the confirmed value back at 500
    // (`Link::process_proof`). The two ends then derived different resource
    // SDUs (464 vs 219), the receiver computed more parts than the
    // advertisement had hashmap entries, and every request it built came back
    // flagged exhausted -- an unbounded 115-byte REQ/HMU loop that burned a PN
    // sync round for 177 s until the outbound watchdog reaped it
    // (`lora_pn_board_sync`, hardware, 2026-09-14). Pinned in
    // `leviculum-core/src/node/mvr_link_mtu_asymmetry.rs`. Read that price
    // before tidying this number back down.
    node.set_interface_hw_mtu(1, 508);
    // The board is the mesh's relay on this link, so it must answer "where
    // is X" for the nodes behind it, not only for the daemon on its serial
    // line (#117 set that one alone). A `Full` interface returns from
    // `handle_path_request` case 3 without re-originating discovery
    // (`Full.discovers_paths() == false`), which left the LoRa and BLE sides
    // of a board deaf to path requests and the announce-capped queue -- 2 %
    // of link capacity, ~13.4 s per relayed announce at SF7 -- as the only
    // way anything could be resolved across the carrier seam. That cost
    // `ble_lora_transport` its `file_transfer` step: the host's path request
    // reached the pocket board over BLE and produced no action, and the
    // listener's announce arrived 91 s later, 1.2 s after the step's 60 s
    // budget. Re-origination excludes the receiving interface, so this adds
    // no airtime to the band the question came in on. Pinned in
    // `leviculum-core/src/node/mvr_board_radio_pathresolve.rs`.
    node.set_interface_mode(1, leviculum_core::InterfaceMode::Gateway);
    node.set_interface_name(2, alloc::string::String::from("ble"));
    node.set_interface_hw_mtu(2, 564);
    node.set_interface_mode(2, leviculum_core::InterfaceMode::Gateway);

    // #234: boot-critical, because `log_fmt` DROPS a line while the
    // runtime-drain gate is shut — it returns before the ring buffer and
    // before the reset-surviving tail — and the gate opens on the first
    // DTR-assert or after 30 s, both long after this point. A one-shot
    // line written there is not late, it is gone, and nothing re-emits it.
    // The full hashes are no longer spelled out here at all: the
    // `[IDENTITY]` banner below carries every one of them, on the critical
    // path and repeated every 5 s, so a reader attached at any moment gets
    // them instead of only one attached across the boot.
    let hash = node.identity().hash();
    log_critical!(
        "LNode started -- identity: {:02X}{:02X}{:02X}{:02X}{:02X}",
        hash[0],
        hash[1],
        hash[2],
        hash[3],
        hash[4]
    );
}
