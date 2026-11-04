//! Firmware: `leviculum-automation` Echo vozlišče za Seeed XIAO nRF52840 +
//! Wio-SX1262 kit.
//!
//! Najmanjši možen automation-device firmware: `App<Echo>` + `NoIo`.
//! Remote node pošlje `ECHC` params frame (poljubni bajti, do 32) v
//! podpisanem LXMF sporočilu → board ga aplicira → naslednji `ECHS`
//! report vrne iste bajte nazaj. Celoten LXMF krog (podpis, decode,
//! report, dostava, target learning) testiran z enim klicem.
//!
//! Razlike od `lxmf_echo.rs`:
//!   - brez `LxmfNode` inboxa — `on_message` sam razpakira opportunistični
//!     paket (delivery destination registrirana BREZ predala)
//!   - echo logika ne živi v binary-ju ampak v `leviculum_automation::echo`
//!   - params "blob" je RAM-only (Echo nima config-a za shranjevanje —
//!     ECHC payload je podatek, ne konfiguracija)
//!
//! Gradnja:
//!   cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin auto_echo
//! Flash:
//!   tools/uf2-runner.sh target/thumbv7em-none-eabihf/release/auto_echo

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_nrf::gpio::Level;
use embassy_nrf::spim;
use embassy_time::{Duration, Instant, Timer};

use leviculum_automation::echo::{Echo, EchoReport};
use leviculum_automation::hal::ConfigStore;
use leviculum_automation::{App, Inbound, NoIo};
use leviculum_core::embedded_storage::EmbeddedStorage;
use leviculum_core::ifac::IfacConfig;
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::traits::{Interface, Storage};
use leviculum_core::transport::dispatch_actions;
use leviculum_core::{DestinationHash, InterfaceId, ProofStrategy};
use leviculum_lxmf::LxmfNode;

use leviculum_nrf::automation::{EmbassyClock as AutoClock, FirmwareLog};
use leviculum_nrf::ble::{BleInterface, PeerEvent as BlePeerEvent};
use leviculum_nrf::boards::xiaokit;
use leviculum_nrf::clock::EmbassyClock;
use leviculum_nrf::interface::EmbeddedInterface;
use leviculum_nrf::lora::LoRaInterface;
use leviculum_nrf::{info, init_heap, log_critical};

// ---------------------------------------------------------------------------
// Params store — RAM-only.
//
// `ECHC` je podatek za echo, ne konfiguracija: po rebootu je naslednje remote
// sporočilo vseeno pravi odgovor, zato se ne splača pisati v flash (na
// SoftDevice buildu pa bi NVMC pisanje iz zanke bilo nelegalno).
// ---------------------------------------------------------------------------
struct RamStore(Option<Vec<u8>>);
impl ConfigStore for RamStore {
    fn load(&mut self) -> Option<Vec<u8>> {
        self.0.clone()
    }
    fn save(&mut self, blob: &[u8]) -> bool {
        self.0 = Some(blob.to_vec());
        true
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // --- 1. boot diagnostika -------------------------------------------------
    let boot = leviculum_nrf::boot_trace::capture();
    let mut config = embassy_nrf::config::Config::default();
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let mut p = embassy_nrf::init(config);

    init_heap();
    leviculum_nrf::init_tracing();
    let hardfault_pm = leviculum_nrf::take_hardfault_postmortem();
    let panic_pm = leviculum_nrf::take_panic_postmortem();
    let _persistent_log = leviculum_nrf::log::take_persistent_log();
    unsafe { leviculum_nrf::paint_stack() }
    leviculum_nrf::set_panic_led(
        xiaokit::PANIC_LED_PORT,
        xiaokit::PANIC_LED_PIN,
        xiaokit::PANIC_LED_ACTIVE_LOW,
    );
    leviculum_nrf::set_hardfault_led(0, 6, true);
    leviculum_nrf::set_irq_priorities();

    let (media, media_src) =
        leviculum_nrf::media::load_at_boot(xiaokit::CONFIG.telemetry_flash_page);
    leviculum_nrf::name::load_at_boot(xiaokit::CONFIG.telemetry_flash_page);
    let boot_count = leviculum_nrf::boot_count::record_at_boot(
        embassy_nrf::nvmc::Nvmc::new(p.NVMC.reborrow()),
        &boot,
    );
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::PersistRead);

    // --- 2. USB serial -------------------------------------------------------
    let vbus = leviculum_nrf::init_vbus();
    let serial = leviculum_nrf::usb::init(&spawner, p.USBD, vbus, &xiaokit::CONFIG);
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::UsbUp);

    log_critical!("leviculum automation echo booting (xiaokit)");
    log_critical!("[FW_BUILD] {}", leviculum_nrf::FW_BUILD_STAMP);
    leviculum_nrf::log_stack("boot");
    leviculum_nrf::log_panic_count();
    leviculum_nrf::boot_trace::log_prev(&boot);
    leviculum_nrf::log_reset_reason(boot.reset_reason);
    leviculum_nrf::boot_count::log_banner(&boot_count);
    leviculum_nrf::log_irq_priorities();
    leviculum_nrf::log_postmortems(hardfault_pm.as_ref(), panic_pm.as_ref());

    let _hichg = xiaokit::high_charge(p.P0_13);
    let mut led = xiaokit::led(p.P0_30);
    let rng = leviculum_nrf::rng::RawHwRng::new();

    // --- 3. identiteta -------------------------------------------------------
    let mut id_store = leviculum_nrf::flash::NvmcIdentityStore::new(
        embassy_nrf::nvmc::Nvmc::new(p.NVMC),
        xiaokit::CONFIG.identity_flash_page,
    );
    const MAX_ENDPOINT_LINKS: usize =
        leviculum_nrf::heap_census::max_endpoint_links(core::mem::size_of::<
            leviculum_core::node::NodeCore<
                leviculum_nrf::rng::RawHwRng,
                EmbassyClock,
                EmbeddedStorage,
            >,
        >());
    const _: () = assert!(
        MAX_ENDPOINT_LINKS >= leviculum_nrf::ble::MAX_LINKS,
        "HEAP_BUDGET affords fewer endpoint links than claimable BLE sessions"
    );

    let mut builder = NodeCoreBuilder::new()
        .enable_transport(true)
        .max_links(Some(MAX_ENDPOINT_LINKS))
        .max_incoming_resource_size(leviculum_nrf::MAX_INCOMING_RESOURCE_BYTES)
        .max_queued_announces(8)
        .max_random_blobs(8)
        .respond_to_probes(true);

    let identity_loaded = {
        use leviculum_core::identity_store::IdentityStore;
        if let Ok(Some(identity)) = id_store.load() {
            log_critical!("Identity loaded from flash");
            builder = builder.identity(identity);
            true
        } else {
            log_critical!("No identity in flash, generating new");
            false
        }
    };

    let mut node = builder.build_boxed(rng, EmbassyClock, EmbeddedStorage::new());
    leviculum_nrf::heap_census::log_budget_and_assert(core::mem::size_of_val(&*node));

    if !identity_loaded {
        use leviculum_core::identity_store::IdentityStore;
        let _ = id_store.save(node.identity());
        info!("Identity saved to flash");
    }

    // --- 4. vmesniki ----------------------------------------------------------
    node.set_interface_name(0, alloc::string::String::from("serial_usb"));
    node.set_interface_hw_mtu(0, 564);
    node.set_interface_mode(0, leviculum_core::InterfaceMode::Gateway);
    node.set_interface_name(1, alloc::string::String::from("lora_sx1262"));
    node.set_interface_hw_mtu(1, 508);
    node.set_interface_mode(1, leviculum_core::InterfaceMode::Gateway);
    node.set_interface_name(2, alloc::string::String::from("ble"));
    node.set_interface_hw_mtu(2, 564);
    node.set_interface_mode(2, leviculum_core::InterfaceMode::Gateway);

    let identity_hash = *node.identity().hash();
    let identity = node.identity().clone();
    log_critical!(
        "LNode started -- identity: \
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        identity_hash[0], identity_hash[1], identity_hash[2], identity_hash[3],
        identity_hash[4], identity_hash[5], identity_hash[6], identity_hash[7],
        identity_hash[8], identity_hash[9], identity_hash[10], identity_hash[11],
        identity_hash[12], identity_hash[13], identity_hash[14], identity_hash[15],
    );

    // --- 5. LoRa --------------------------------------------------------------
    log_critical!("[STG] lora-init");
    let lora = leviculum_nrf::lora::init(
        p.SPI2,
        p.P1_13.into(),
        p.P1_15.into(),
        p.P1_14.into(),
        p.P0_04.into(),
        p.P0_28.into(),
        p.P0_29.into(),
        p.P0_03.into(),
        Some(p.P0_05.into()),
        spim::Frequency::M4,
        xiaokit::CONFIG.lora_tcxo_voltage_reg,
    )
    .await;
    info!("SX1262 ready");

    let radio_cfg =
        leviculum_nrf::radio_store::load(xiaokit::CONFIG.radio_config_flash_page)
            .and_then(leviculum_nrf::lora::RadioConfig::from_wire_config)
            .unwrap_or_else(leviculum_nrf::lora::RadioConfig::eu_medium);
    let lora_channels = leviculum_nrf::lora::channels();
    if media.lora_enabled {
        let channel_seed = leviculum_nrf::lora::channel_seed();
        spawner.must_spawn(leviculum_nrf::lora::lora_task(lora, radio_cfg, channel_seed));
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraTask);
    } else {
        leviculum_nrf::media::log_carrier_held_down("lora");
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraSkipped);
    }

    // --- 6. BLE ---------------------------------------------------------------
    leviculum_nrf::name::note_boot_name(&identity_hash);
    log_critical!("[STG] ble-init");
    let sd = leviculum_nrf::ble::init(
        &spawner,
        media.ble_enabled,
        identity_hash,
        vbus,
        p.RTC0,
        p.TIMER0,
        p.TEMP,
        p.PPI_CH19,
        p.PPI_CH30,
        p.PPI_CH31,
        p.PPI_CH17,
        p.PPI_CH18,
        p.PPI_CH20,
        p.PPI_CH21,
        p.PPI_CH22,
        p.PPI_CH23,
        p.PPI_CH24,
        p.PPI_CH25,
        p.PPI_CH26,
        p.PPI_CH27,
        p.PPI_CH28,
        p.PPI_CH29,
        p.RNG,
    );
    let ble_channels = leviculum_nrf::ble::channels();
    info!("BLE ready");

    leviculum_nrf::media::note_boot_state(media.lora_enabled, media.ble_enabled);
    leviculum_nrf::media::log_banner(media_src);
    leviculum_nrf::name::log_banner();

    let shared_flash = leviculum_nrf::flash::shared_flash(sd);
    leviculum_nrf::radio_store::spawn_store_task(
        &spawner,
        shared_flash,
        xiaokit::CONFIG.radio_config_flash_page,
    );
    spawner.must_spawn(leviculum_nrf::heap_watermark_task());
    spawner.must_spawn(leviculum_nrf::stack_watermark_task());
    leviculum_nrf::log_stack("post-init");

    // --- 7. delivery destination (BREZ predala) -------------------------------
    //
    // `App<Echo>` sam razpakira opportunistični LXMF paket, zato ne rabimo
    // LxmfNode inbox-a — registriramo samo `lxmf.delivery` destination z
    // `ProofStrategy::App`, da potrdimo dostavo samo ko `on_message` sprejme
    // veljaven `ECHC` okvir.
    let mut delivery_dest =
        LxmfNode::delivery_destination_without_inbox(node.identity().clone())
            .expect("[ECHO] delivery destination build failed");
    delivery_dest.set_proof_strategy(ProofStrategy::App);
    let delivery_hash = *delivery_dest.hash();
    node.register_destination(delivery_dest);
    let our_dest = *delivery_hash.as_bytes();
    leviculum_nrf::identity::note_boot_identity(
        *node.identity().hash(),
        node.probe_dest_hash().map(|h| *h.as_bytes()),
        Some(our_dest),
    );
    {
        let dh = our_dest;
        log_critical!(
            "[ECHO] delivery_hash=\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            dh[0], dh[1], dh[2], dh[3], dh[4], dh[5], dh[6], dh[7],
            dh[8], dh[9], dh[10], dh[11], dh[12], dh[13], dh[14], dh[15],
        );
    }

    // --- 8. adapterji + App<Echo> --------------------------------------------
    let serial_ctl_tx = serial.outgoing_tx;
    let mut serial_iface = EmbeddedInterface::new(serial.outgoing_tx);
    let mut lora_iface = LoRaInterface::new(lora_channels.outgoing_tx);
    let mut ble_iface = BleInterface::new(ble_channels.outgoing_tx);
    let ifac_configs: BTreeMap<usize, IfacConfig> = BTreeMap::new();
    let mut announce_gate = leviculum_nrf::announce::AnnounceGate::new(node.now_ms());

    led.set_level(Level::Low);
    for _ in 0..12_000_000u32 {
        cortex_m::asm::nop();
    }
    led.set_level(Level::High);

    let mut store = RamStore(None);
    let mut alog = FirmwareLog;
    let mut app = App::boot(Echo::new(), &mut store, node.now_ms(), &mut alog);
    let auto_clock = AutoClock;
    let mut pending_report: Option<(EchoReport, bool)> = None;

    let mut transport_stats = leviculum_nrf::transport_stats::Ticker::new();
    let mut heap_census = leviculum_nrf::heap_census::Ticker::new();
    let mut announce_cap = leviculum_nrf::lora::AnnounceCap::new();

    log_critical!("[STG] main-loop");
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::MainLoop);

    // Procesira NodeEvent-e: `PacketReceived` za našo destinacijo → on_message.
    macro_rules! auto_step {
        ($events:expr) => {{
            leviculum_nrf::events::log_events($events, node.now_ms());
            let mut applied_packet = false;
            for ev in $events.iter() {
                if let NodeEvent::AnnounceReceived { announce, .. } = ev {
                    leviculum_nrf::announce::note_announce_heard(
                        node.now_ms(),
                        announce.destination_hash().as_bytes(),
                    );
                    let actions = announce_gate.periodic(&mut node, Some(&delivery_hash));
                    if !actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                        let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                        leviculum_nrf::dispatch::settle("announce-neighbour", &mut node, &dispatched);
                    }
                }
                if let NodeEvent::PacketReceived { destination, data, .. } = ev {
                    if destination.as_bytes() == &our_dest {
                        let signer = leviculum_automation::lxmf::peek_source(data)
                            .and_then(|src| node.storage().get_identity(&src));
                        match app.on_message(
                            data,
                            &our_dest,
                            node.now_ms(),
                            &mut alog,
                            signer,
                        ) {
                            Inbound::Applied { blob, .. } => {
                                store.save(&blob);
                                applied_packet = true;
                            }
                            Inbound::Rejected => {
                                log_critical!("[ECHO] params rejected");
                            }
                            Inbound::Ignored => {}
                        }
                    }
                }
            }
            if applied_packet {
                for ev in $events.iter() {
                    if let NodeEvent::PacketProofRequested {
                        packet_hash,
                        destination_hash,
                        interface_index,
                        peer,
                    } = ev
                    {
                        if destination_hash.as_bytes() == &our_dest {
                            if let Ok(out) = node.send_proof_on_peer(
                                packet_hash,
                                destination_hash,
                                *interface_index,
                                *peer,
                            ) {
                                let mut ifaces: [&mut dyn Interface; 3] =
                                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                                let dispatched =
                                    dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                                leviculum_nrf::dispatch::settle("echo-proof", &mut node, &dispatched);
                            }
                        }
                    }
                }
            }
        }};
    }

    loop {
        transport_stats.poll(&node);
        leviculum_nrf::events::flush_drop_summary(node.now_ms());
        heap_census.poll(&node, None);

        let deadline = node
            .next_deadline()
            .map(Instant::from_millis)
            .unwrap_or(Instant::MAX)
            .min(transport_stats.deadline())
            .min(heap_census.deadline())
            .min(Instant::from_millis(node.now_ms() + 250)); // echo cadence

        let announce_wait_ms = announce_gate.periodic_wait_ms(node.now_ms()).max(1);
        announce_cap.sync(&mut node);

        let wake = select4(
            select4(
                serial.incoming_rx.receive(),
                lora_channels.incoming_rx.receive(),
                select(
                    ble_channels.incoming_rx.receive(),
                    ble_channels.peer_event_rx.receive(),
                ),
                Timer::at(deadline),
            ),
            Timer::after(Duration::from_millis(announce_wait_ms)),
            serial.wall_time_rx.receive(),
            serial.announce_rx.receive(),
        )
        .await;

        node.set_interface_online(0, serial_iface.is_online());
        node.set_interface_online(1, lora_iface.is_online());
        node.set_interface_online(2, ble_iface.is_online());
        node.set_interface_peer_count(2, ble_iface.peer_count());
        if leviculum_nrf::set_time_source(node.time_source()) {
            log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
        }

        match wake {
            Either4::Second(()) => {
                let actions = announce_gate.periodic(&mut node, Some(&delivery_hash));
                if !actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                    leviculum_nrf::dispatch::settle("announce-periodic", &mut node, &dispatched);
                }
            }
            Either4::Third(unix_secs) => {
                use leviculum_core::envelope;
                use leviculum_core::transport::TimeSource;
                let answer = if node.set_wall_time_unix_secs(unix_secs, TimeSource::Host) {
                    leviculum_nrf::set_time_source(TimeSource::Host);
                    log_critical!("[TIME_SEED] source=host unix={}", unix_secs);
                    log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
                    envelope::encode_ack(envelope::TYPE_WALL_TIME)
                } else {
                    log_critical!("[TIME_SEED_REFUSED] source=host unix={}", unix_secs);
                    envelope::encode_refusal(envelope::TYPE_WALL_TIME, envelope::REFUSE_VALUE)
                };
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::Fourth(()) => {
                use leviculum_core::envelope;
                let answer = if !node.has_plausible_wall_clock() {
                    log_critical!("[ANNOUNCE] withheld reason=no-clock");
                    envelope::encode_refusal(envelope::TYPE_ANNOUNCE, envelope::REFUSE_NO_CLOCK)
                } else {
                    let app_data = leviculum_nrf::telemetry::announce_app_data(node.identity());
                    match node.announce_destination(&delivery_hash, Some(&app_data)) {
                        Ok(out) => {
                            let mut ifaces: [&mut dyn Interface; 3] =
                                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                            let dispatched =
                                dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                            leviculum_nrf::dispatch::settle("announce-host", &mut node, &dispatched);
                            let dh = delivery_hash.as_bytes();
                            log_critical!(
                                "[ANNOUNCE] sent dst={:02x}{:02x}{:02x}{:02x} reason=host",
                                dh[0], dh[1], dh[2], dh[3]
                            );
                            envelope::encode_ack(envelope::TYPE_ANNOUNCE)
                        }
                        Err(_) => envelope::encode_refusal(
                            envelope::TYPE_ANNOUNCE,
                            envelope::REFUSE_BUSY,
                        ),
                    }
                };
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::First(Either4::First(data)) => {
                info!("SER RX {} bytes", data.len());
                let output = node.handle_packet(InterfaceId(0), &data);
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ser-rx", &mut node, &dispatched);
                auto_step!(&output.events);
            }
            Either4::First(Either4::Second(data)) => {
                if !leviculum_nrf::media::lora_active() {
                    continue;
                }
                let output = node.handle_packet(InterfaceId(1), &data);
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("lora-rx", &mut node, &dispatched);
                auto_step!(&output.events);
            }
            Either4::First(Either4::Third(Either::First((peer, data)))) => {
                leviculum_nrf::ble::incoming_held_sub(data.capacity());
                if !leviculum_nrf::media::ble_active() {
                    continue;
                }
                let output = match peer {
                    Some(peer) => node.handle_packet_from_peer(InterfaceId(2), peer, &data),
                    None => node.handle_packet(InterfaceId(2), &data),
                };
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ble-rx", &mut node, &dispatched);
                auto_step!(&output.events);
            }
            Either4::First(Either4::Third(Either::Second(event))) => {
                let (label, output) = match event {
                    BlePeerEvent::Lost(peer) => {
                        ("ble-peer-lost", node.handle_interface_peer_lost(InterfaceId(2), peer))
                    }
                    BlePeerEvent::Up(peer) => {
                        let mut o = node.handle_interface_peer_up(InterfaceId(2), peer);
                        o.actions
                            .extend(announce_gate.peer_up(&mut node, Some(&delivery_hash), peer));
                        ("ble-peer-up", o)
                    }
                };
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle(label, &mut node, &dispatched);
                auto_step!(&output.events);
            }
            Either4::First(Either4::Fourth(())) => {
                let output = node.handle_timeout();
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("timeout", &mut node, &dispatched);
                auto_step!(&output.events);
            }
        }

        // Echo tick: ko je report due → signed opportunistic LXMF reply.
        let tick = app.tick(&mut NoIo, &auto_clock, &mut alog);
        if let Some(report) = tick.report {
            pending_report = Some((report, false));
        }

        if let Some((report, ref mut path_requested)) = pending_report {
            if let Some(target) = app.target() {
                if let Some(on_air) =
                    app.report_message(&identity, &our_dest, &report, node.now_ms())
                {
                    let target_hash = DestinationHash::new(target);
                    match node.send_single_packet(&target_hash, &on_air) {
                        Ok((_, out)) => {
                            let mut ifaces: [&mut dyn Interface; 3] =
                                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                            let dispatched =
                                dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                            leviculum_nrf::dispatch::settle("echo-tx", &mut node, &dispatched);
                            log_critical!(
                                "[ECHO] reply sent dest={:02x}{:02x}{:02x}{:02x}",
                                target[0], target[1], target[2], target[3]
                            );
                            pending_report = None;
                        }
                        Err(_) => {
                            if !*path_requested {
                                *path_requested = true;
                                log_critical!(
                                    "[ECHO] reply deferred (requesting path) dest={:02x}{:02x}{:02x}{:02x}",
                                    target[0], target[1], target[2], target[3]
                                );
                                let out = node.request_path(&target_hash);
                                if !out.actions.is_empty() {
                                    let mut ifaces: [&mut dyn Interface; 3] =
                                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                                    let dispatched =
                                        dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                                    leviculum_nrf::dispatch::settle(
                                        "echo-path-req",
                                        &mut node,
                                        &dispatched,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
