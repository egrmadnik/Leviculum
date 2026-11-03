//! Firmware: LXMF echo vozlišče za Seeed XIAO nRF52840 + Wio-SX1262 kit
//!
//! Primer implementacije LXMF inbox-a na vgradnem vozlišču. Sprejema sporočila
//! prek treh vmesnikov (LoRa SX1262, BLE, USB CDC-ACM) in vsakemu pošiljatelju
//! odgovori z "echo: <izvirno sporočilo>".
//!
//! Razlike od `xiaokit.rs`:
//!   - namesto "delivery destination without inbox" (samo announce) registrira
//!     POLNI poštni predal (LxmfNode::register), ki sprejema tako packet kot
//!     resource dostave ter direktne povezave
//!   - ni telemetry reporterja, ni PN engine-a, ni GNSS/battery inicializacije
//!   - doda `LxmfNode` stanje in echo logiko
//!
//! Gradnja:
//!   cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin lxmf_echo
//!
//! Flash:
//!   just flash-xiaokit-lxmf-echo   (doda v Justfile)
//!   # ali direktno:
//!   lnflash -b lxmf_echo.uf2

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

use leviculum_core::embedded_storage::EmbeddedStorage;
use leviculum_core::ifac::IfacConfig;
use leviculum_core::node::{NodeCoreBuilder, NodeEvent};
use leviculum_core::traits::Interface;
use leviculum_core::transport::dispatch_actions;
use leviculum_core::InterfaceId;

// LXMF protokol — delivery inbox in gradnja sporočil
// Vse ključne tipe so re-exportane na vrhnjem nivoju leviculum_lxmf.
use leviculum_lxmf::{
    DeliveryMethod, LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent, Message,
};

use leviculum_nrf::ble::{BleInterface, PeerEvent as BlePeerEvent};
use leviculum_nrf::boards::xiaokit;
use leviculum_nrf::clock::EmbassyClock;
use leviculum_nrf::interface::EmbeddedInterface;
use leviculum_nrf::lora::LoRaInterface;
use leviculum_nrf::{info, init_heap, log_critical};

// ---------------------------------------------------------------------------
// Čakajoči echo odgovor
// ---------------------------------------------------------------------------

/// Echo odgovor, ki čaka, ker pot do prejemnika še ni znana.
///
/// Ko pride `PeerAnnounced` ali `DirectLinkEstablished` za ta hash,
/// `flush_pending_echoes` odda odgovor.
struct PendingEcho {
    /// 16-bajtni hash dostavljalnega odhodišča pošiljatelja.
    dest: [u8; 16],
    /// Vsebina echo odgovora (že sestavljena: "echo: " + original content).
    content: Vec<u8>,
    /// LXMF timestamp iz prejetega sporočila (unix sekunde kot f64).
    timestamp: f64,
}

// ---------------------------------------------------------------------------
// Embassy entry point
// ---------------------------------------------------------------------------

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // -----------------------------------------------------------------------
    // 1. Boot diagnostika (mora biti PRVA — bere RESETREAS in breadcrumbs
    //    prejšnjega zagona, preden jih prepišemo)
    // -----------------------------------------------------------------------
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

    // SAFETY: klicano enkrat, pred vsemi opravili.
    unsafe { leviculum_nrf::paint_stack() }

    // LEDi za diagnostiko (active LOW na XIAO RGB).
    leviculum_nrf::set_panic_led(
        xiaokit::PANIC_LED_PORT,
        xiaokit::PANIC_LED_PIN,
        xiaokit::PANIC_LED_ACTIVE_LOW,
    );
    leviculum_nrf::set_hardfault_led(0, 6, true); // modra LED = hardfault
    leviculum_nrf::set_irq_priorities();

    // Preberemo medijsko konfiguracijo in ime vozlišča iz flash-a.
    let (media, media_src) =
        leviculum_nrf::media::load_at_boot(xiaokit::CONFIG.telemetry_flash_page);
    leviculum_nrf::name::load_at_boot(xiaokit::CONFIG.telemetry_flash_page);

    // Boot štetje — pred Softdevice::enable (pisanje v notranji flash je
    // legalno le prek SoftDevice-a ko ta teče).
    let boot_count = leviculum_nrf::boot_count::record_at_boot(
        embassy_nrf::nvmc::Nvmc::new(p.NVMC.reborrow()),
        &boot,
    );
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::PersistRead);

    // -----------------------------------------------------------------------
    // 2. USB serial (debug + Reticulum host vmesnik)
    // -----------------------------------------------------------------------
    let vbus = leviculum_nrf::init_vbus();
    let serial = leviculum_nrf::usb::init(&spawner, p.USBD, vbus, &xiaokit::CONFIG);
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::UsbUp);

    log_critical!("leviculum LXMF echo booting (xiaokit)");
    log_critical!("[FW_BUILD] {}", leviculum_nrf::FW_BUILD_STAMP);
    leviculum_nrf::log_stack("boot");
    leviculum_nrf::log_panic_count();
    leviculum_nrf::boot_trace::log_prev(&boot);
    leviculum_nrf::log_reset_reason(boot.reset_reason);
    leviculum_nrf::boot_count::log_banner(&boot_count);
    leviculum_nrf::log_irq_priorities();
    leviculum_nrf::log_postmortems(hardfault_pm.as_ref(), panic_pm.as_ref());

    // ISET na P0.13 nizko → 100 mA polnjenje baterije (enako kot xiaokit).
    let _hichg = xiaokit::high_charge(p.P0_13);
    let mut led = xiaokit::led(p.P0_30); // zelena LED (active LOW)

    let rng = leviculum_nrf::rng::RawHwRng::new();

    // -----------------------------------------------------------------------
    // 3. Identiteta vozlišča (naložimo iz flash-a ali generiramo novo)
    // -----------------------------------------------------------------------
    let mut id_store = leviculum_nrf::flash::NvmcIdentityStore::new(
        embassy_nrf::nvmc::Nvmc::new(p.NVMC),
        xiaokit::CONFIG.identity_flash_page,
    );

    // Kompajlirna preverjanja heap budgeta (#388).
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
    const _: () = assert!(
        leviculum_nrf::heap_census::budget_total(core::mem::size_of::<
            leviculum_core::node::NodeCore<
                leviculum_nrf::rng::RawHwRng,
                EmbassyClock,
                EmbeddedStorage,
            >,
        >()) <= leviculum_nrf::HEAP_SIZE,
        "HEAP_BUDGET total exceeds the heap: shrink a term"
    );

    if !identity_loaded {
        use leviculum_core::identity_store::IdentityStore;
        let _ = id_store.save(node.identity());
        info!("Identity saved to flash");
    }

    // -----------------------------------------------------------------------
    // 4. Vmesniki (serial=0, lora=1, ble=2)
    // -----------------------------------------------------------------------
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
    log_critical!(
        "LNode started -- identity: \
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        identity_hash[0],  identity_hash[1],  identity_hash[2],  identity_hash[3],
        identity_hash[4],  identity_hash[5],  identity_hash[6],  identity_hash[7],
        identity_hash[8],  identity_hash[9],  identity_hash[10], identity_hash[11],
        identity_hash[12], identity_hash[13], identity_hash[14], identity_hash[15],
    );

    // -----------------------------------------------------------------------
    // 5. SX1262 LoRa radio (na SPIM2, pini iz boards/xiaokit.rs)
    // -----------------------------------------------------------------------
    log_critical!("[STG] lora-init");
    let lora = leviculum_nrf::lora::init(
        p.SPI2,
        p.P1_13.into(), // SCK  (D8)
        p.P1_15.into(), // MOSI (D10)
        p.P1_14.into(), // MISO (D9)
        p.P0_04.into(), // NSS  (D4) — chip select, active LOW
        p.P0_28.into(), // RESET(D2)
        p.P0_29.into(), // BUSY (D3)
        p.P0_03.into(), // DIO1 (D1)
        Some(p.P0_05.into()), // RXEN (D5) — antenna switch, high = RX
        spim::Frequency::M4,
        xiaokit::CONFIG.lora_tcxo_voltage_reg,
    )
    .await;
    info!("SX1262 ready");

    let radio_cfg =
        leviculum_nrf::radio_store::load(xiaokit::CONFIG.radio_config_flash_page)
            .and_then(leviculum_nrf::lora::RadioConfig::from_wire_config)
            .unwrap_or_else(|| {
                leviculum_nrf::log::log_fmt("[RADIO] ", format_args!("default eu_medium"));
                leviculum_nrf::lora::RadioConfig::eu_medium()
            });

    let lora_channels = leviculum_nrf::lora::channels();
    if media.lora_enabled {
        let channel_seed = leviculum_nrf::lora::channel_seed();
        spawner
            .must_spawn(leviculum_nrf::lora::lora_task(lora, radio_cfg, channel_seed));
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraTask);
    } else {
        leviculum_nrf::media::log_carrier_held_down("lora");
        leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::LoraSkipped);
    }

    // -----------------------------------------------------------------------
    // 6. BLE (Nordic S140 SoftDevice)
    // -----------------------------------------------------------------------
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

    // Radio konfiguracijski flash (SD flash mora biti po enable-u).
    let shared_flash = leviculum_nrf::flash::shared_flash(sd);
    leviculum_nrf::radio_store::spawn_store_task(
        &spawner,
        shared_flash,
        xiaokit::CONFIG.radio_config_flash_page,
    );

    spawner.must_spawn(leviculum_nrf::heap_watermark_task());
    spawner.must_spawn(leviculum_nrf::stack_watermark_task());

    let (hu, hf) = leviculum_nrf::heap_stats();
    info!("heap u={} f={}", hu, hf);
    leviculum_nrf::log_stack("post-init");

    // -----------------------------------------------------------------------
    // 7. LXMF inbox (srce tega binary-ja)
    // -----------------------------------------------------------------------
    //
    // `LxmfNode::delivery_destination` ustvari `lxmf.delivery` odhodišče z:
    //   • ProofStrategy::All  → vsakemu pošiljatelju potrdimo prejem
    //   • accepts_links = true → sprejemamo direktne LXMF povezave (za večja
    //     sporočila, ki ne gredo v en paket)
    //
    // `LxmfNode::register` to odhodišče prijavi v NodeCore in vrne LxmfNode,
    // ki v `handle_event` razpakira vsa prihajajoča sporočila in jih odda
    // kot `LxmfNodeEvent::MessageReceived`.

    let delivery_dest =
        LxmfNode::delivery_destination(node.identity().clone())
            .expect("[LXMF] delivery destination build failed");
    let delivery_hash = *delivery_dest.hash();

    // Na vgradnih napravah izključimo bzip2 kompresijo (manjši flash).
    let lxmf_config = LxmfNodeConfig {
        max_incoming_resource_size: Some(leviculum_nrf::MAX_INCOMING_RESOURCE_BYTES as u64),
        max_outgoing_delivery_size: Some(leviculum_nrf::MAX_INCOMING_RESOURCE_BYTES as u64),
        auto_compress_resources: false,
    };
    let mut lxmf = LxmfNode::register(&mut node, delivery_dest, lxmf_config)
        .expect("[LXMF] node registration failed");

    {
        let dh = delivery_hash.as_bytes();
        log_critical!(
            "[LXMF] inbox ready -- delivery_hash=\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            dh[0],  dh[1],  dh[2],  dh[3],
            dh[4],  dh[5],  dh[6],  dh[7],
            dh[8],  dh[9],  dh[10], dh[11],
            dh[12], dh[13], dh[14], dh[15],
        );
    }

    // Announce gate bo interno sestavil app_data prek telemetry::announce_app_data.
    // Eksplicitna `DeliveryAnnounce` ni potrebna, ker jo `AnnounceGate` zgradi sam.

    // -----------------------------------------------------------------------
    // 8. Vmesniški adapterji in stanje glavne zanke
    // -----------------------------------------------------------------------
    let mut serial_iface = EmbeddedInterface::new(serial.outgoing_tx);
    let mut lora_iface = LoRaInterface::new(lora_channels.outgoing_tx);
    let mut ble_iface = BleInterface::new(ble_channels.outgoing_tx);
    let ifac_configs: BTreeMap<usize, IfacConfig> = BTreeMap::new();

    // Announce gate: skrbi za periodične najave in throttling (#376, #401).
    let mut announce_gate = leviculum_nrf::announce::AnnounceGate::new(node.now_ms());

    // Kratek boot blink (active LOW = LOW prižge zeleno LED).
    led.set_level(Level::Low);
    for _ in 0..12_000_000u32 {
        cortex_m::asm::nop();
    }
    led.set_level(Level::High);

    // Queue za echo odgovore, ki čakajo na pot do prejemnika.
    let mut pending_echoes: Vec<PendingEcho> = Vec::new();

    let mut transport_stats = leviculum_nrf::transport_stats::Ticker::new();
    let mut heap_census = leviculum_nrf::heap_census::Ticker::new();
    let mut announce_cap = leviculum_nrf::lora::AnnounceCap::new();

    log_critical!("[STG] main-loop");
    leviculum_nrf::boot_trace::phase(leviculum_nrf::boot_trace::Phase::MainLoop);

    // -----------------------------------------------------------------------
    // Makro: procesira NodeEvent-e skozi LxmfNode in obravnava echo logiko.
    //
    // Za vsak NodeEvent pokliče `lxmf.handle_event`, razpošlje morebitne
    // LXMF akcije (proof-i, resource potrditve, ...) in obravnava LXMF
    // aplikacijske dogodke:
    //   - MessageReceived → sestavi echo odgovor in ga pošlji
    //   - PeerAnnounced   → poizkusi sprazni čakajočo vrsto
    //   - DirectLinkEstablished → enako
    // -----------------------------------------------------------------------
    macro_rules! lxmf_step {
        ($events:expr) => {{
            // Sporoča announce cadu, da smo slišali soseda (#401).
            leviculum_nrf::events::log_events($events, node.now_ms());

            for ev in $events.iter() {
                // Obvestimo announce gate o sosedovih najavah.
                if let NodeEvent::AnnounceReceived { announce, .. } = ev {
                    leviculum_nrf::announce::note_announce_heard(
                        node.now_ms(),
                        announce.destination_hash().as_bytes(),
                    );
                }

                // Predamo vsak NodeEvent LxmfNode-u.
                let lxmf_out = match lxmf.handle_event(&mut node, ev) {
                    Ok(out) => out,
                    Err(_) => continue,
                };

                // Razpošljemo LXMF-jedrne akcije (potrjevanje, link setup…).
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

                // Obravnavamo LXMF aplikacijske dogodke.
                for lxmf_ev in lxmf_out.events {
                    match lxmf_ev {
                        // -----------------------------------------------
                        // PREJETO SPOROČILO → echo odgovor
                        // -----------------------------------------------
                        LxmfNodeEvent::MessageReceived(msg) => {
                            // Izpis vsebine (UTF-8 ali hex za binarne podatke)
                            match core::str::from_utf8(&msg.content) {
                                Ok(s) => log_critical!(
                                    "[LXMF] msg rx src={:02x}{:02x}{:02x}{:02x} \
                                     len={} verify={:?} content={:?}",
                                    msg.source_hash[0],
                                    msg.source_hash[1],
                                    msg.source_hash[2],
                                    msg.source_hash[3],
                                    msg.content.len(),
                                    msg.verification,
                                    s,
                                ),
                                Err(_) => log_critical!(
                                    "[LXMF] msg rx src={:02x}{:02x}{:02x}{:02x} \
                                     len={} verify={:?} content=<binary>",
                                    msg.source_hash[0],
                                    msg.source_hash[1],
                                    msg.source_hash[2],
                                    msg.source_hash[3],
                                    msg.content.len(),
                                    msg.verification,
                                ),
                            };
                            send_echo_or_queue(
                                &mut node,
                                &mut lxmf,
                                msg.source_hash,
                                &msg.content,
                                msg.timestamp,
                                &delivery_hash,
                                &mut serial_iface,
                                &mut lora_iface,
                                &mut ble_iface,
                                &ifac_configs,
                                &mut pending_echoes,
                            );
                        }

                        // -----------------------------------------------
                        // Sporočilo zavrnjeno (neveljavno, preveliko, ...)
                        // -----------------------------------------------
                        LxmfNodeEvent::InboundRejected { method, reason } => {
                            log_critical!(
                                "[LXMF] inbound rejected method={:?} reason={}",
                                method,
                                reason
                            );
                        }

                        // -----------------------------------------------
                        // Sorodnik se je najavil ali je bila vzpostavljena
                        // direktna povezava → poizkusimo sprazni vrsto
                        // -----------------------------------------------
                        LxmfNodeEvent::PeerAnnounced { destination } => {
                            {
                                let b = destination.as_bytes();
                                log_critical!(
                                    "[LXMF] peer announced {:02x}{:02x}{:02x}{:02x}",
                                    b[0], b[1], b[2], b[3]
                                );
                            }
                            flush_pending_echoes(
                                &mut node,
                                &mut lxmf,
                                &delivery_hash,
                                &mut pending_echoes,
                                &mut serial_iface,
                                &mut lora_iface,
                                &mut ble_iface,
                                &ifac_configs,
                            );
                        }

                        LxmfNodeEvent::DirectLinkEstablished {
                            destination,
                            is_initiator,
                            ..
                        } => {
                            log_critical!(
                                "[LXMF] link established dest={} initiator={}",
                                destination
                                    .map(|d| {
                                        let b = d.as_bytes();
                                        alloc::format!(
                                            "{:02x}{:02x}{:02x}{:02x}",
                                            b[0], b[1], b[2], b[3]
                                        )
                                    })
                                    .unwrap_or_else(|| alloc::string::String::from("?")),
                                is_initiator,
                            );
                            flush_pending_echoes(
                                &mut node,
                                &mut lxmf,
                                &delivery_hash,
                                &mut pending_echoes,
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
        }};
    }

    // -----------------------------------------------------------------------
    // GLAVNA ZANKA
    // -----------------------------------------------------------------------
    loop {
        transport_stats.poll(&node);
        leviculum_nrf::events::flush_drop_summary(node.now_ms());
        heap_census.poll(&node, None);

        let deadline = node
            .next_deadline()
            .map(Instant::from_millis)
            .unwrap_or(Instant::MAX)
            .min(transport_stats.deadline())
            .min(heap_census.deadline());

        let announce_wait_ms = announce_gate.periodic_wait_ms(node.now_ms()).max(1);

        announce_cap.sync(&mut node);
        if leviculum_nrf::set_time_source(node.time_source()) {
            log_critical!("[TIME_SOURCE] source={}", leviculum_nrf::time_source_str());
        }

        // Čakamo na katerikoli vir: serial, LoRa, BLE paketi ali tajmaut.
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
            // Nerabljeni viri: GNSS, host kontrolni ukazi, telemetrija.
            // Ohranimo enak select4(select4(...)) vzorec kot xiaokit.rs.
            core::future::pending::<()>(),
            core::future::pending::<()>(),
        )
        .await;

        // Zrcalimo online stanje vmesnikov v jedro (za pravilno usmerjanje).
        node.set_interface_online(0, serial_iface.is_online());
        node.set_interface_online(1, lora_iface.is_online());
        node.set_interface_online(2, ble_iface.is_online());
        node.set_interface_peer_count(2, ble_iface.peer_count());

        match wake {
            // ------------------------------------------------------------------
            // Periodična najava (announce) — announce_gate odloči kdaj
            // ------------------------------------------------------------------
            Either4::Second(()) => {
                // Najavimo delivery destination na vseh vmesnikih.
                let actions = announce_gate.periodic(&mut node, Some(&delivery_hash));
                if !actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                    leviculum_nrf::dispatch::settle("announce-periodic", &mut node, &dispatched);
                }
            }

            // ------------------------------------------------------------------
            // Prispeli podatki prek USB serial
            // ------------------------------------------------------------------
            Either4::First(Either4::First(data)) => {
                info!("SER RX {} bytes", data.len());
                let output = node.handle_packet(InterfaceId(0), &data);
                info!("SER RX -> {} actions", output.actions.len());
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ser-rx", &mut node, &dispatched);
                lxmf_step!(&output.events);
            }

            // ------------------------------------------------------------------
            // Prispeli podatki prek LoRa SX1262
            // ------------------------------------------------------------------
            Either4::First(Either4::Second(data)) => {
                // Ignoriramo, če je LoRa izklopljeno v medijskem profilu.
                if !leviculum_nrf::media::lora_active() {
                    continue;
                }
                let output = node.handle_packet(InterfaceId(1), &data);
                if !output.actions.is_empty() {
                    info!("LORA RX -> {} actions", output.actions.len());
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("lora-rx", &mut node, &dispatched);
                lxmf_step!(&output.events);
            }

            // ------------------------------------------------------------------
            // Prispeli podatki prek BLE
            // ------------------------------------------------------------------
            Either4::First(Either4::Third(Either::First((peer, data)))) => {
                leviculum_nrf::ble::incoming_held_sub(data.capacity());
                if !leviculum_nrf::media::ble_active() {
                    continue;
                }
                let output = match peer {
                    Some(peer) => node.handle_packet_from_peer(InterfaceId(2), peer, &data),
                    None => node.handle_packet(InterfaceId(2), &data),
                };
                if !output.actions.is_empty() {
                    info!("BLE RX -> {} actions", output.actions.len());
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("ble-rx", &mut node, &dispatched);
                lxmf_step!(&output.events);
            }

            // ------------------------------------------------------------------
            // BLE peer vzpostavitev / izguba
            // ------------------------------------------------------------------
            Either4::First(Either4::Third(Either::Second(event))) => {
                let (label, output) = match event {
                    BlePeerEvent::Lost(peer) => {
                        let o = node.handle_interface_peer_lost(InterfaceId(2), peer);
                        info!(
                            "BLE peer lost, {} paths culled",
                            o.events
                                .iter()
                                .filter(|e| matches!(e, NodeEvent::PathLost { .. }))
                                .count()
                        );
                        ("ble-peer-lost", o)
                    }
                    BlePeerEvent::Up(peer) => {
                        let mut o = node.handle_interface_peer_up(InterfaceId(2), peer);
                        info!("BLE peer up, {} pull actions", o.actions.len());
                        // Najavimo se novemu BLE peeru (na tej povezavi).
                        o.actions.extend(
                            announce_gate.peer_up(&mut node, Some(&delivery_hash), peer),
                        );
                        ("ble-peer-up", o)
                    }
                };
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle(label, &mut node, &dispatched);
                lxmf_step!(&output.events);
            }

            // ------------------------------------------------------------------
            // Protokolni tajmaut (keepalive, link timeouts, resource timeouts…)
            // ------------------------------------------------------------------
            Either4::First(Either4::Fourth(())) => {
                let output = node.handle_timeout();
                if !output.actions.is_empty() {
                    info!("timeout: {} actions", output.actions.len());
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                leviculum_nrf::dispatch::settle("timeout", &mut node, &dispatched);
                lxmf_step!(&output.events);
            }

            Either4::Third(()) | Either4::Fourth(()) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Echo logika
// ---------------------------------------------------------------------------

/// Sestavi LXMF echo sporočilo in ga pošlji, ali ga postavi v čakajočo vrsto.
///
/// Algoritem:
/// 1. Če imamo odprto direktno LXMF povezavo do pošiljatelja → Direct dostava
/// 2. Sicer → Opportunistic (en paket, zahteva pot v usmerjevalni tabeli)
/// 3. Ob napaki → shrani v `pending` in zahtevaj pot/link
#[allow(clippy::too_many_arguments)]
fn send_echo_or_queue(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        leviculum_nrf::clock::EmbassyClock,
        leviculum_core::embedded_storage::EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    dest: [u8; 16],
    original_content: &[u8],
    // Timestamp iz prejetega sporočila — uporabimo kot podpis časa v echo odgovoru.
    received_timestamp: f64,
    delivery_hash: &leviculum_core::DestinationHash,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
    pending: &mut Vec<PendingEcho>,
) {
    // Sestavimo vsebino echo odgovora.
    let mut content = Vec::with_capacity(6 + original_content.len());
    content.extend_from_slice(b"echo: ");
    content.extend_from_slice(original_content);

    match try_send_echo(
        node,
        lxmf,
        dest,
        &content,
        received_timestamp,
        delivery_hash,
        serial_iface,
        lora_iface,
        ble_iface,
        ifac_configs,
    ) {
        EchoResult::Sent => {
            log_critical!(
                "[LXMF] echo sent dest={:02x}{:02x}{:02x}{:02x}",
                dest[0], dest[1], dest[2], dest[3]
            );
        }
        EchoResult::Queued => {
            log_critical!(
                "[LXMF] echo queued (no path) dest={:02x}{:02x}{:02x}{:02x}",
                dest[0], dest[1], dest[2], dest[3]
            );
            pending.push(PendingEcho { dest, content, timestamp: received_timestamp });
        }
        EchoResult::Failed(reason) => {
            log_critical!(
                "[LXMF] echo FAILED dest={:02x}{:02x}{:02x}{:02x} reason={}",
                dest[0], dest[1], dest[2], dest[3], reason
            );
        }
    }
}

/// Rezultat poskusa pošiljanja echo odgovora.
enum EchoResult {
    Sent,
    /// Pot do prejemnika še ni znana; sporočilo naj gre v čakajočo vrsto.
    Queued,
    /// Trajna napaka (pokvarjeno sporočilo ipd.), ni smisla čakati.
    Failed(&'static str),
}

/// Poizkusi sestaviti in poslati echo odgovor.
#[allow(clippy::too_many_arguments)]
fn try_send_echo(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        leviculum_nrf::clock::EmbassyClock,
        leviculum_core::embedded_storage::EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    dest: [u8; 16],
    content: &[u8],
    // Timestamp iz prejetega sporočila — posredujemo ga v echo.
    timestamp: f64,
    delivery_hash: &leviculum_core::DestinationHash,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
) -> EchoResult {
    let dest_dh = leviculum_core::DestinationHash::new(dest);

    // Izberemo metodo glede na razpoložljivo pot.
    let method = if lxmf.direct_link(&dest_dh).is_some() {
        DeliveryMethod::Direct       // odprta direktna LXMF povezava
    } else {
        DeliveryMethod::Opportunistic // en single paket prek routing tabele
    };

    // Naš delivery hash je source sporočila.
    let our_hash = *delivery_hash.as_bytes();

    // Sestavimo in podpišemo sporočilo z identiteto tega vozlišča.
    // `node.identity()` vrne &Identity — borrow se konča preden kličemo
    // `lxmf.send(&mut node, ...)`, torej ni konflikta.
    let reply = Message::create(
        dest,                      // destination: pošiljateljev delivery hash
        our_hash,                  // source: naš delivery hash
        node.identity(),           // podpišemo z našim ključem
        timestamp,
        b"echo".to_vec(),          // title
        content.to_vec(),          // content: "echo: " + original
        Vec::new(),                // brez dodatnih LXMF polj
        method,
    );

    let reply = match reply {
        Ok(r) => r,
        Err(_) => return EchoResult::Failed("message build failed"),
    };

    // Pošljemo sporočilo.
    match lxmf.send(node, &reply) {
        Ok(out) => {
            if !out.core.actions.is_empty() {
                let mut ifaces: [&mut dyn Interface; 3] =
                    [serial_iface, lora_iface, ble_iface];
                let dispatched =
                    dispatch_actions(&mut ifaces, out.core.actions, ifac_configs);
                leviculum_nrf::dispatch::settle("lxmf-echo-tx", node, &dispatched);
            }
            EchoResult::Sent
        }

        // Direktna povezava ni bila aktivna (link se je že zaprl ali ga ni) —
        // poskusimo opportunistic.
        Err(LxmfNodeError::DirectLinkUnavailable) if method == DeliveryMethod::Direct => {
            log_critical!("[LXMF] direct link gone, retry opportunistic");
            // Ponovi z Opportunistic metodo.
            let reply2 = Message::create(
                dest,
                our_hash,
                node.identity(),
                timestamp,
                b"echo".to_vec(),
                content.to_vec(),
                Vec::new(),
                DeliveryMethod::Opportunistic,
            );
            match reply2 {
                Ok(r) => match lxmf.send(node, &r) {
                    Ok(out) => {
                        if !out.core.actions.is_empty() {
                            let mut ifaces: [&mut dyn Interface; 3] =
                                [serial_iface, lora_iface, ble_iface];
                            let dispatched =
                                dispatch_actions(&mut ifaces, out.core.actions, ifac_configs);
                            leviculum_nrf::dispatch::settle("lxmf-echo-tx2", node, &dispatched);
                        }
                        EchoResult::Sent
                    }
                    Err(e) => queue_or_fail(lxmf, node, dest_dh, serial_iface, lora_iface, ble_iface, ifac_configs, e),
                },
                Err(_) => EchoResult::Failed("message build (retry) failed"),
            }
        }

        // Pot ni znana → zahtevamo pot in vrsta.
        Err(e) => queue_or_fail(lxmf, node, dest_dh, serial_iface, lora_iface, ble_iface, ifac_configs, e),
    }
}

/// Skupna logika: ko pošiljanje ne uspe, poizkusimo zahtevati pot in
/// sporočilo dodamo v čakajočo vrsto (ali ga zavržemo pri tranji napaki).
#[allow(clippy::too_many_arguments)]
fn queue_or_fail(
    lxmf: &mut LxmfNode,
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        leviculum_nrf::clock::EmbassyClock,
        leviculum_core::embedded_storage::EmbeddedStorage,
    >,
    dest_dh: leviculum_core::DestinationHash,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
    err: LxmfNodeError,
) -> EchoResult {
    log_critical!("[LXMF] send err={:?} queuing+path-req", err);
    match err {
        // Napake pri katerih ima smisel čakati na pot.
        LxmfNodeError::Send(_)
        | LxmfNodeError::UnknownPeer
        | LxmfNodeError::DirectLinkUnavailable => {
            match lxmf.ensure_direct_link(node, dest_dh) {
                Ok((_, path_out)) if !path_out.core.actions.is_empty() => {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [serial_iface, lora_iface, ble_iface];
                    let dispatched =
                        dispatch_actions(&mut ifaces, path_out.core.actions, ifac_configs);
                    leviculum_nrf::dispatch::settle("lxmf-path-req", node, &dispatched);
                }
                // UnknownPeer: nimamo ključa → pošljemo path_request direktno
                // da relay vrne announce s ključem prejemnika.
                Err(LxmfNodeError::UnknownPeer) | _ => {
                    let pr = node.request_path(&dest_dh);
                    if !pr.actions.is_empty() {
                        log_critical!(
                            "[LXMF] path_req dest={:02x}{:02x}{:02x}{:02x}",
                            dest_dh.as_bytes()[0],
                            dest_dh.as_bytes()[1],
                            dest_dh.as_bytes()[2],
                            dest_dh.as_bytes()[3],
                        );
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [serial_iface, lora_iface, ble_iface];
                        let dispatched =
                            dispatch_actions(&mut ifaces, pr.actions, ifac_configs);
                        leviculum_nrf::dispatch::settle("lxmf-path-req2", node, &dispatched);
                    }
                }
            }
            EchoResult::Queued
        }
        // Trajna napaka (npr. preveliko sporočilo) — zavržemo.
        _ => EchoResult::Failed("permanent send error"),
    }
}

/// Poizkusi poslati vse čakajoče echo odgovore.
///
/// Kličemo po `PeerAnnounced`, `DirectLinkEstablished` in ob vsakem
/// tajmautu, da čakajoči odgovori ne ostanejo v vrsti za vedno.
#[allow(clippy::too_many_arguments)]
fn flush_pending_echoes(
    node: &mut leviculum_core::node::NodeCore<
        leviculum_nrf::rng::RawHwRng,
        leviculum_nrf::clock::EmbassyClock,
        leviculum_core::embedded_storage::EmbeddedStorage,
    >,
    lxmf: &mut LxmfNode,
    delivery_hash: &leviculum_core::DestinationHash,
    pending: &mut Vec<PendingEcho>,
    serial_iface: &mut EmbeddedInterface,
    lora_iface: &mut LoRaInterface,
    ble_iface: &mut BleInterface,
    ifac_configs: &BTreeMap<usize, IfacConfig>,
) {
    // Obdržimo le tiste, ki jih še nismo uspeli poslati.
    pending.retain(|p| {
        match try_send_echo(
            node,
            lxmf,
            p.dest,
            &p.content,
            p.timestamp,
            delivery_hash,
            serial_iface,
            lora_iface,
            ble_iface,
            ifac_configs,
        ) {
            EchoResult::Sent => {
                log_critical!(
                    "[LXMF] echo (queued) sent dest={:02x}{:02x}{:02x}{:02x}",
                    p.dest[0], p.dest[1], p.dest[2], p.dest[3]
                );
                false // odstranimo iz vrste
            }
            EchoResult::Queued => true,  // ostane v vrsti
            EchoResult::Failed(_) => false, // trajna napaka — zavržemo
        }
    });
}

// ---------------------------------------------------------------------------
// Opomnik za flashanje in Justfile recept
// ---------------------------------------------------------------------------
//
// Da dodate ta binary v Justfile, dodajte:
//
//   flash-xiaokit-lxmf-echo:
//     cargo build -p leviculum-nrf --release --features bsp-xiaokit --bin lxmf_echo
//     arm-none-eabi-objcopy -O binary \
//       target/thumbv7em-none-eabihf/release/lxmf_echo lxmf_echo.bin
//     # nato prek lnflash ali UF2 bootloader
