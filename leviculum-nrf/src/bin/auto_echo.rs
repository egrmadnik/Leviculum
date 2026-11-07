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
use leviculum_core::node::{NodeCoreBuilder, NodeEvent, SendError};
use leviculum_core::traits::{Interface, Storage};
use leviculum_core::transport::dispatch_actions;
use leviculum_core::{DestinationHash, Identity, InterfaceId};
use leviculum_lxmf::message::DeliveryMethod;
use leviculum_lxmf::{LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent, Message};

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

/// Čakajoči tekstovni echo odgovor (npr. za MeshChatX), ko pot do pošiljatelja
/// še ni v usmerjevalni tabeli.
struct PendingTextEcho {
    dest: [u8; 16],
    content: Vec<u8>,
    timestamp: f64,
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

    // Privzeti radijski profil za hitro lokalno stacionarno mrežo:
    // 869.463 MHz, SF7, BW 250 kHz, CR 4/5, 22 dBm (ujema se z /etc/reticulum/config).
    // Če je vozlišče priključeno na `lnsd` prek USB, lahko `lnsd` te parametre
    // kadarkoli dinamično prepiše prek serijskega kontrolnega okvirja.
    let radio_cfg = leviculum_nrf::lora::RadioConfig {
        frequency_hz: 869_463_000,
        sf: 7,
        bw: 0x05, // 250 kHz (SX1262 Table 14-47)
        cr: 0x01, // CR 4/5
        tx_power_dbm: leviculum_core::rnode::DEFAULT_TX_POWER_DBM,
        preamble_len: leviculum_core::rnode::derive_preamble_symbols(7, 5, 250_000),
        bw_hz: 250_000,
        cr_denom: 5,
        csma_enabled: true,
        radio_silent: false,
        silence_lease_s: 0,
        st_alock: 0,
        lt_alock: 0,
        lt_alock_present: false,
    };
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

    // --- 7. delivery destination + LxmfNode (podpira ECHC/ECHS in MeshChatX) ---
    //
    // Registriramo polno `lxmf.delivery` destinacijo (`accepts_links = true`,
    // `ProofStrategy::All`), da lahko odjemalci kot MeshChatX pošiljajo navadna
    // tekstovna sporočila prek Direct povezave ali Opportunistic paketa, hkrati
    // pa `App<Echo>` obravnava binarne `ECHC` okvirje iz `echo-term.py`.
    let delivery_dest =
        LxmfNode::delivery_destination(node.identity().clone())
            .expect("[ECHO] delivery destination build failed");
    let delivery_hash = *delivery_dest.hash();
    let lxmf_config = LxmfNodeConfig {
        max_incoming_resource_size: Some(leviculum_nrf::MAX_INCOMING_RESOURCE_BYTES as u64),
        max_outgoing_delivery_size: Some(leviculum_nrf::MAX_INCOMING_RESOURCE_BYTES as u64),
        auto_compress_resources: false,
    };
    let mut lxmf = LxmfNode::register(&mut node, delivery_dest, lxmf_config)
        .expect("[ECHO] lxmf node registration failed");
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
    let mut pending_text_echoes: Vec<PendingTextEcho> = Vec::new();
    let mut last_lora_sig: Option<(i16, i16)> = None;
    // `EmbeddedStorage::known_identities` drži le 8 vnosov. Če je gostiteljski
    // `lnsd` povezan z internetnimi TCP vozlišči, tuje najave (hops >= 3) hitro
    // izrinejo identiteto lokalnega odjemalca (`echo-term`, hops <= 2). Zato
    // hranimo do 8 bližnjih identitet + aktivni `app.target()` in jih po potrebi
    // osvežimo v `node.remember_identity`.
    let mut pinned_identities: Vec<([u8; 16], Identity)> = Vec::with_capacity(8);

    let mut transport_stats = leviculum_nrf::transport_stats::Ticker::new();
    let mut heap_census = leviculum_nrf::heap_census::Ticker::new();
    let mut announce_cap = leviculum_nrf::lora::AnnounceCap::new();

    log_critical!("[STG] main-loop");
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::MainLoop);

    // Procesira NodeEvent-e:
    // 1. Če prispe opportunistični `ECHC` okvir (`echo-term.py`), ga obdela `App<Echo>`
    //    in vrne binarni `ECHS` report.
    // 2. Sicer dogodek preda `LxmfNode` (MeshChatX Direct link ali Opportunistic tekst)
    //    in ob `MessageReceived` vrne čitljiv tekstovni odgovor z RSSI/SNR in uptime.
    macro_rules! auto_step {
        ($events:expr, $signal:expr) => {{
            let sig: Option<(i16, i16)> = $signal;
            if sig.is_some() {
                last_lora_sig = sig;
            }
            leviculum_nrf::events::log_events($events, node.now_ms());
            for ev in $events.iter() {
                if let NodeEvent::AnnounceReceived { announce, .. } = ev {
                    let dest_bytes = *announce.destination_hash().as_bytes();
                    leviculum_nrf::announce::note_announce_heard(
                        node.now_ms(),
                        &dest_bytes,
                    );
                    let hops = node
                        .storage()
                        .get_path(&dest_bytes)
                        .map(|p| p.hops)
                        .unwrap_or(u8::MAX);
                    if (hops <= 2 || app.target() == Some(dest_bytes))
                        && announce.to_identity().is_ok()
                    {
                        if let Ok(id) = announce.to_identity() {
                            if let Some((_, slot)) =
                                pinned_identities.iter_mut().find(|(d, _)| *d == dest_bytes)
                            {
                                *slot = id;
                            } else {
                                if pinned_identities.len() >= 8 {
                                    let evict_idx = pinned_identities
                                        .iter()
                                        .position(|(d, _)| Some(*d) != app.target())
                                        .unwrap_or(0);
                                    pinned_identities.remove(evict_idx);
                                }
                                pinned_identities.push((dest_bytes, id));
                            }
                        }
                    }
                    let actions = announce_gate.periodic(&mut node, Some(&delivery_hash));
                    if !actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                        let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                        leviculum_nrf::dispatch::settle("announce-neighbour", &mut node, &dispatched);
                    }
                }

                let mut handled_by_echc = false;
                if let NodeEvent::PacketReceived { destination, data, .. } = ev {
                    if destination.as_bytes() == &our_dest {
                        let src_opt = leviculum_automation::lxmf::peek_source(data);
                        if let Some(src) = src_opt {
                            if node.storage().get_identity(&src).is_none() {
                                if let Some((_, id)) =
                                    pinned_identities.iter().find(|(d, _)| *d == src)
                                {
                                    node.remember_identity(DestinationHash::new(src), id.clone());
                                }
                            }
                        }
                        let signer = src_opt
                            .and_then(|src| node.storage().get_identity(&src).cloned());
                        match app.on_message(
                            data,
                            &our_dest,
                            node.now_ms(),
                            &mut alog,
                            signer.as_ref(),
                        ) {
                            Inbound::Applied { blob, source, .. } => {
                                handled_by_echc = true;
                                let (rssi, snr) = sig.unwrap_or((i16::MIN, i16::MIN));
                                app.device_mut().set_last_signal(rssi, snr);
                                store.save(&blob);
                                if let Some(id) = signer {
                                    if let Some((_, slot)) =
                                        pinned_identities.iter_mut().find(|(d, _)| *d == source)
                                    {
                                        *slot = id;
                                    } else {
                                        if pinned_identities.len() >= 8 {
                                            pinned_identities.remove(0);
                                        }
                                        pinned_identities.push((source, id));
                                    }
                                }
                            }
                            Inbound::Rejected | Inbound::Ignored => {}
                        }
                    }
                }

                if !handled_by_echc {
                    if let Ok(lxmf_out) = lxmf.handle_event(&mut node, ev) {
                        if !lxmf_out.core.actions.is_empty() {
                            let mut ifaces: [&mut dyn Interface; 3] =
                                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                            let dispatched = dispatch_actions(
                                &mut ifaces,
                                lxmf_out.core.actions,
                                &ifac_configs,
                            );
                            leviculum_nrf::dispatch::settle("lxmf", &mut node, &dispatched);
                        }
                        for lxmf_ev in lxmf_out.events {
                            match lxmf_ev {
                                LxmfNodeEvent::MessageReceived(msg) => {
                                    let reply_bytes = format_text_echo(
                                        &msg.content,
                                        sig.or(last_lora_sig),
                                        (node.now_ms() / 1000) as u32,
                                    );
                                    log_critical!(
                                        "[ECHO] text rx src={:02x}{:02x}{:02x}{:02x} len={}",
                                        msg.source_hash[0],
                                        msg.source_hash[1],
                                        msg.source_hash[2],
                                        msg.source_hash[3],
                                        msg.content.len(),
                                    );
                                    send_text_echo_or_queue(
                                        &mut node,
                                        &mut lxmf,
                                        &pinned_identities,
                                        msg.source_hash,
                                        reply_bytes,
                                        msg.timestamp,
                                        &delivery_hash,
                                        &mut serial_iface,
                                        &mut lora_iface,
                                        &mut ble_iface,
                                        &ifac_configs,
                                        &mut pending_text_echoes,
                                    );
                                }
                                LxmfNodeEvent::PeerAnnounced { .. }
                                | LxmfNodeEvent::DirectLinkEstablished { .. } => {
                                    flush_pending_text_echoes(
                                        &mut node,
                                        &mut lxmf,
                                        &pinned_identities,
                                        &delivery_hash,
                                        &mut pending_text_echoes,
                                        &mut serial_iface,
                                        &mut lora_iface,
                                        &mut ble_iface,
                                        &ifac_configs,
                                    );
                                }
                                _ => {}
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
                auto_step!(&output.events, None);
            }
            Either4::First(Either4::Second(data)) => {
                if !leviculum_nrf::media::lora_active() {
                    continue;
                }
                let lora_sig =
                    leviculum_nrf::lora::last_reception().map(|r| (r.rssi_dbm, r.snr_db));
                let output = node.handle_packet(InterfaceId(1), &data);
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("lora-rx", &mut node, &dispatched);
                auto_step!(&output.events, lora_sig);
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
                auto_step!(&output.events, None);
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
                auto_step!(&output.events, None);
            }
                Either4::First(Either4::Fourth(())) => {
                let output = node.handle_timeout();
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("timeout", &mut node, &dispatched);
                auto_step!(&output.events, None);
                if !pending_text_echoes.is_empty() {
                    flush_pending_text_echoes(
                        &mut node,
                        &mut lxmf,
                        &pinned_identities,
                        &delivery_hash,
                        &mut pending_text_echoes,
                        &mut serial_iface,
                        &mut lora_iface,
                        &mut ble_iface,
                        &ifac_configs,
                    );
                }
            }
        }

        // Echo tick: ko je report due → signed opportunistic LXMF reply.
        let tick = app.tick(&mut NoIo, &auto_clock, &mut alog);
        if let Some(report) = tick.report {
            pending_report = Some((report, false));
        }

        if let Some((report, ref mut path_requested)) = pending_report {
            if let Some(target) = app.target() {
                if node.storage().get_identity(&target).is_none() {
                    if let Some((_, id)) = pinned_identities.iter().find(|(d, _)| *d == target) {
                        node.remember_identity(DestinationHash::new(target), id.clone());
                    }
                }
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
                        Err(SendError::PacingDelay { .. }) => {
                            // Vmesnik čaka na prosto oddajno okno — poskusimo v naslednjem ciklu.
                        }
                        Err(_) => {
                            if !*path_requested {
                                *path_requested = true;
                                log_critical!(
                                    "[ECHO] reply deferred (requesting path) dest={:02x}{:02x}{:02x}{:02x}",
                                    target[0], target[1], target[2], target[3]
                                );
                                // Odstranimo morebitno staro pot iz `path_table`, da predpomnjen
                                // odgovor `lnsd` ne pade na preverjanju `AnnounceReplay`.
                                let _ = node.remove_path(&target);
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

/// Sestavi čitljiv tekstovni odgovor za odjemalce kot MeshChatX:
/// `echo: <besedilo> [rssi=-48dBm snr=11dB up=120s]` (oz. `[up=120s]` brez LoRa).
fn format_text_echo(original: &[u8], sig: Option<(i16, i16)>, uptime_s: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(original.len() + 48);
    out.extend_from_slice(b"echo: ");
    out.extend_from_slice(original);
    let tail = match sig {
        Some((rssi, snr)) if rssi != i16::MIN => {
            alloc::format!(" [rssi={}dBm snr={}dB up={}s]", rssi, snr, uptime_s)
        }
        _ => alloc::format!(" [up={}s]", uptime_s),
    };
    out.extend_from_slice(tail.as_bytes());
    out
}

enum TextEchoResult {
    Sent,
    Queued,
    Failed,
}

#[allow(clippy::too_many_arguments)]
fn send_text_echo_or_queue(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        EmbassyClock,
        EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    pinned_identities: &[([u8; 16], Identity)],
    dest: [u8; 16],
    content: Vec<u8>,
    timestamp: f64,
    delivery_hash: &DestinationHash,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
    pending: &mut Vec<PendingTextEcho>,
) {
    match try_send_text_echo(
        node,
        lxmf,
        pinned_identities,
        dest,
        &content,
        timestamp,
        delivery_hash,
        serial_iface,
        lora_iface,
        ble_iface,
        ifac_configs,
    ) {
        TextEchoResult::Sent => {
            log_critical!(
                "[ECHO] text reply sent dest={:02x}{:02x}{:02x}{:02x}",
                dest[0], dest[1], dest[2], dest[3]
            );
        }
        TextEchoResult::Queued => {
            log_critical!(
                "[ECHO] text reply queued dest={:02x}{:02x}{:02x}{:02x}",
                dest[0], dest[1], dest[2], dest[3]
            );
            pending.push(PendingTextEcho {
                dest,
                content,
                timestamp,
            });
        }
        TextEchoResult::Failed => {
            log_critical!(
                "[ECHO] text reply failed dest={:02x}{:02x}{:02x}{:02x}",
                dest[0], dest[1], dest[2], dest[3]
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_send_text_echo(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        EmbassyClock,
        EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    pinned_identities: &[([u8; 16], Identity)],
    dest: [u8; 16],
    content: &[u8],
    timestamp: f64,
    delivery_hash: &DestinationHash,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
) -> TextEchoResult {
    if node.storage().get_identity(&dest).is_none() {
        if let Some((_, id)) = pinned_identities.iter().find(|(d, _)| *d == dest) {
            node.remember_identity(DestinationHash::new(dest), id.clone());
        }
    }
    let dest_dh = DestinationHash::new(dest);
    let method = if lxmf.direct_link(&dest_dh).is_some() {
        DeliveryMethod::Direct
    } else {
        DeliveryMethod::Opportunistic
    };
    let our_hash = *delivery_hash.as_bytes();
    let Ok(reply) = Message::create(
        dest,
        our_hash,
        node.identity(),
        timestamp,
        b"echo".to_vec(),
        content.to_vec(),
        Vec::new(),
        method,
    ) else {
        return TextEchoResult::Failed;
    };

    match lxmf.send(node, &reply) {
        Ok(out) => {
            if !out.core.actions.is_empty() {
                let mut ifaces: [&mut dyn Interface; 3] = [serial_iface, lora_iface, ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, out.core.actions, ifac_configs);
                leviculum_nrf::dispatch::settle("text-echo-tx", node, &dispatched);
            }
            TextEchoResult::Sent
        }
        Err(LxmfNodeError::DirectLinkUnavailable) if method == DeliveryMethod::Direct => {
            let Ok(reply2) = Message::create(
                dest,
                our_hash,
                node.identity(),
                timestamp,
                b"echo".to_vec(),
                content.to_vec(),
                Vec::new(),
                DeliveryMethod::Opportunistic,
            ) else {
                return TextEchoResult::Failed;
            };
            match lxmf.send(node, &reply2) {
                Ok(out) => {
                    if !out.core.actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [serial_iface, lora_iface, ble_iface];
                        let dispatched =
                            dispatch_actions(&mut ifaces, out.core.actions, ifac_configs);
                        leviculum_nrf::dispatch::settle("text-echo-tx2", node, &dispatched);
                    }
                    TextEchoResult::Sent
                }
                Err(_) => {
                    let _ = node.remove_path(&dest);
                    let pr = node.request_path(&dest_dh);
                    if !pr.actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [serial_iface, lora_iface, ble_iface];
                        let dispatched = dispatch_actions(&mut ifaces, pr.actions, ifac_configs);
                        leviculum_nrf::dispatch::settle("text-echo-pr", node, &dispatched);
                    }
                    TextEchoResult::Queued
                }
            }
        }
        Err(
            LxmfNodeError::Send(_)
            | LxmfNodeError::UnknownPeer
            | LxmfNodeError::DirectLinkUnavailable,
        ) => {
            let _ = node.remove_path(&dest);
            let pr = node.request_path(&dest_dh);
            if !pr.actions.is_empty() {
                let mut ifaces: [&mut dyn Interface; 3] = [serial_iface, lora_iface, ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, pr.actions, ifac_configs);
                leviculum_nrf::dispatch::settle("text-echo-pr", node, &dispatched);
            }
            TextEchoResult::Queued
        }
        Err(_) => TextEchoResult::Failed,
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_pending_text_echoes(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        EmbassyClock,
        EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    pinned_identities: &[([u8; 16], Identity)],
    delivery_hash: &DestinationHash,
    pending: &mut Vec<PendingTextEcho>,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
) {
    pending.retain(|p| {
        match try_send_text_echo(
            node,
            lxmf,
            pinned_identities,
            p.dest,
            &p.content,
            p.timestamp,
            delivery_hash,
            serial_iface,
            lora_iface,
            ble_iface,
            ifac_configs,
        ) {
            TextEchoResult::Sent => {
                log_critical!(
                    "[ECHO] text reply (queued) sent dest={:02x}{:02x}{:02x}{:02x}",
                    p.dest[0], p.dest[1], p.dest[2], p.dest[3]
                );
                false
            }
            TextEchoResult::Queued => true,
            TextEchoResult::Failed => false,
        }
    });
}
