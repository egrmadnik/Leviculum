//! The event loop of an LNode: one cooperative task that owns the
//! transport node and reacts to whatever wakes it.
//!
//! Setup (delivery destination, propagation role, announce policy,
//! telemetry reporter) happens once at the top of [`run`]; the loop then
//! never returns.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Instant, Timer};

use leviculum_core::ifac::IfacConfig;
use leviculum_core::node::NodeEvent;
use leviculum_core::traits::Interface;
use leviculum_core::transport::{dispatch_actions, Action};
use leviculum_core::InterfaceId;

use crate::ble::{BleChannels, BleInterface, PeerEvent as BlePeerEvent};
use crate::interface::EmbeddedInterface;
use crate::lora::{LoRaChannels, LoRaInterface};
use crate::node::board::Board;
use crate::node::readings;
use crate::node::reticulum::Node;
use crate::usb::SerialChannels;
use crate::{info, log_critical};

/// How often the telemetry policy is asked whether a report is due.
///
/// This is a *poll* rate, not a cadence: the cadence lives in the profile
/// and is minutes to hours. Five seconds is fine enough that "report now"
/// means now to an operator watching a serial log, and coarse enough that
/// it is invisible next to the tasks already waking on the same period.
const TELEMETRY_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Log each action's discriminant and target. A bring-up diagnostic: it
/// tells whether a rebroadcast emitted a Broadcast (hits all interfaces)
/// or a SendPacket (single interface only).
fn log_actions(actions: &[Action]) {
                    for act in actions {
                        match act {
                            Action::SendPacket { iface, data, peer } => {
                                info!(
                                    "ACT SendPacket iface={} len={} routed={}",
                                    iface.0,
                                    data.len(),
                                    peer.is_some()
                                );
                            }
                            Action::Broadcast {
                                exclude_iface,
                                exclude_peer,
                                data,
                                ..
                            } => {
                                let excl = exclude_iface.map(|i| i.0 as i16).unwrap_or(-1);
                                // The LINK the exclusion spares, when the
                                // interface named one (#422): `excl=2
                                // excl_link=-` is the whole BLE interface
                                // silenced, which is the shape the rig cell
                                // died of.
                                match exclude_peer {
                                    Some(peer) => info!(
                                        "ACT Broadcast excl={} excl_link={:02x}{:02x}{:02x}{:02x} len={}",
                                        excl,
                                        peer[0],
                                        peer[1],
                                        peer[2],
                                        peer[3],
                                        data.len()
                                    ),
                                    None => info!(
                                        "ACT Broadcast excl={} excl_link=- len={}",
                                        excl,
                                        data.len()
                                    ),
                                }
                            }
                        }
                    }
}

/// Run the node. Never returns.
pub async fn run<B: Board>(
    spawner: &Spawner,
    mut node: Box<Node>,
    serial: SerialChannels,
    lora_channels: LoRaChannels,
    ble_channels: BleChannels,
    sd: &'static nrf_softdevice::Softdevice,
) -> ! {
    // Interface adapters
    let serial_ctl_tx = serial.outgoing_tx;
    let mut serial_iface = EmbeddedInterface::new(serial.outgoing_tx);
    let mut lora_iface = LoRaInterface::new(lora_channels.outgoing_tx);
    let mut ble_iface = BleInterface::new(ble_channels.outgoing_tx);
    let ifac_configs: BTreeMap<usize, IfacConfig> = BTreeMap::new();

    // Telemetry (Codeberg #236). The delivery destination is registered
    // unconditionally, target or not: it is what a receiver verifies our
    // LXMF signature against, and it is useful on its own — a node that
    // announces it can be addressed by name instead of by hex string.
    // With the `lxmf-echo` or `lxmf-blink` feature the full inbox takes the
    // announce-only destination's place (same identity, same hash).
    #[cfg(feature = "lxmf-echo")]
    let mut app = crate::node::responder::Responder::register(
        &mut node,
        crate::node::echo::TITLE,
        crate::node::echo::reply,
    );
    #[cfg(feature = "lxmf-blink")]
    let mut app = crate::node::responder::Responder::register(
        &mut node,
        crate::node::blink::TITLE,
        crate::node::blink::reply,
    );
    #[cfg(any(feature = "lxmf-echo", feature = "lxmf-blink"))]
    let delivery_hash = app.as_ref().map(|e| e.delivery_hash());
    #[cfg(not(any(feature = "lxmf-echo", feature = "lxmf-blink")))]
    let delivery_hash = crate::telemetry::register_delivery_destination(&mut node);
    // Every hash a prober needs, on one boot-critical line (and from
    // here on in the periodic banner and the identity query). This is
    // the only place they are written: a per-BSP copy on the gated path
    // was a line a late reader never saw (#234).
    crate::identity::note_boot_identity(
        *node.identity().hash(),
        node.probe_dest_hash().map(|h| *h.as_bytes()),
        delivery_hash.as_ref().map(|h| *h.as_bytes()),
    );
    // The propagation-node role (#384 part 3): register the
    // `lxmf.propagation` destination and its two request handlers, over
    // the record-log store the store task mounts. Costs from the config
    // page (PN_CONFIG line above states which source answered). The
    // miner task is the peering-key grinder — cooperative, off the main
    // loop, results harvested by the engine.
    let pn_config = crate::pn::load_config_at_boot(B::CONFIG.telemetry_flash_page);
    let mut pn_engine = crate::pn::Engine::new(&mut node, pn_config);
    if let Some(pn) = pn_engine.as_ref() {
        crate::identity::note_propagation(*pn.destination_hash().as_bytes());
        spawner.must_spawn(crate::pn::miner_task());
    } else {
        log_critical!("PN role=off reason=identity-underivable");
    }

    // The announce occasions telemetry does not cover (#376): a new BLE
    // peer, and the plain timer. Armed from the node's clock so the first
    // periodic announce is a fixed delay after boot, not after whatever
    // the loop happened to do first.
    let mut announce_gate = crate::announce::AnnounceGate::new(node.now_ms());
    let mut reporter = delivery_hash.map(crate::telemetry::Reporter::new);
    if reporter.is_none() {
        log_critical!("[TELEMETRY] target=00000000 state=off reason=no-delivery-destination");
    }
    if let Some(reporter) = reporter.as_mut() {
        // A persisted target comes back as awaiting-key unless its key
        // was persisted with it; the node then resolves it over the air
        // exactly as it would after a fresh set. The reporter rewrites a
        // hash-only record with the key once it is resolved (#370), so
        // after one successful resolution a reboot restores ready and
        // owes the immediate report.
        if let Some(stored) = crate::telemetry::load(B::CONFIG.telemetry_flash_page) {
            reporter.apply_target(&mut node, stored);
        }
        // A persisted fixed position replaces the sensor from the first
        // report of this boot on — the pin must not depend on which
        // record the host set last.
        if let Some(stored) =
            crate::telemetry::load_fixed_position(B::CONFIG.telemetry_flash_page)
        {
            reporter.apply_fixed_position(Some(stored));
        }
        reporter.log_banner();
    }
    let telemetry_target_rx = crate::telemetry::inbound_target_receiver();
    let fixed_position_rx = crate::telemetry::inbound_fixed_position_receiver();

    // Calendar seeding from GNSS (Codeberg #166 item 1, #69 on this
    // board): the main loop owns the node, so it is the one place a fix
    // can reach the wall-time seam. One accepted fix seeds; the monotonic
    // clock carries the calendar from there — the receiver keeps running
    // for position only, never as a clock.
    #[cfg(feature = "gnss")]
    let mut gnss_rx = {
        let rx = crate::baseboard::GNSS_FIX.receiver();
        if rx.is_none() {
            log_critical!("[TIME_SEED_REFUSED] source=gnss reason=watch_capacity");
        }
        rx
    };
    #[cfg(feature = "gnss")]
    let mut time_seed_gate = leviculum_gnss_time::SeedGate::new();

    // Periodic `[TRANSPORT]` counters (#344). Rides the main loop rather than
    // a spawned task: the counters live in the node this loop owns.
    let mut transport_stats = crate::transport_stats::Ticker::new();

    // The `[HEAP_CENSUS]` line (#388): who holds the heap. Rides the
    // main loop like the `[TRANSPORT]` ticker — the node and the engine
    // it walks are this loop's own state.
    let mut heap_census = crate::heap_census::Ticker::new();

    // The announce bandwidth cap's price list (#402). The core has always had
    // the cap — queue, fewest-hops-first drain, 2 % share — but it only binds
    // an interface somebody registered a bitrate for, and the firmware
    // registered none, so the medium where an announce is DEAREST was the one
    // medium that never held one back. Synced after every wake because the
    // PHY changes under it (the test cells reconfigure the radio per profile);
    // the tracker registers only on a real change.
    let mut announce_cap = crate::lora::AnnounceCap::new();

    log_critical!("[STG] main-loop");
    crate::boot_trace::phase(crate::boot_trace::Phase::MainLoop);

    // One engine pass: digest a dispatch's events, run the async half
    // (validation, flash flushes, periodic jobs), and put whatever the
    // role wants on the wire. A macro because the borrows are the loop's
    // own locals; expanded only after a dispatch, never inside the
    // select, so the engine's awaits cannot be cancelled (the DropBomb
    // rule, `crate::record_store` module docs).
    macro_rules! pn_step {
        ($events:expr) => {
            // Board-visible core-event lines (LINK_REFUSED, #388) render
            // regardless of whether a propagation role runs.
            crate::events::log_events($events, node.now_ms());
            // #235: answer a `/status` request from an allow-listed identity.
            // In this macro because every arm that produces core events runs
            // it, and a remote query may arrive over any of the three
            // carriers — a responder wired into one arm would answer only the
            // carrier its author had in mind.
            {
                let mut status_out = leviculum_core::transport::TickOutput::empty();
                crate::remote_status::handle_events(&mut node, $events, &mut status_out);
                if !status_out.actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched =
                        dispatch_actions(&mut ifaces, status_out.actions, &ifac_configs);
                    crate::dispatch::settle("remote-status", &mut node, &dispatched);
                }
            }
            #[cfg(any(feature = "lxmf-echo", feature = "lxmf-blink"))]
            if let Some(app) = app.as_mut() {
                let mut io = crate::node::responder::Ifaces {
                    serial: &mut serial_iface,
                    lora: &mut lora_iface,
                    ble: &mut ble_iface,
                    ifac: &ifac_configs,
                };
                app.on_events(&mut node, $events, &mut io);
            }
            if let Some(pn) = pn_engine.as_mut() {
                let mut pn_out = pn.on_events(&mut node, $events);
                pn_out.merge(pn.settle(&mut node).await);
                if !pn_out.actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched = dispatch_actions(&mut ifaces, pn_out.actions, &ifac_configs);
                    crate::dispatch::settle("pn", &mut node, &dispatched);
                }
            }
        };
    }
    // Event-driven main loop, nine event sources:
    // 1. Serial incoming (USB)
    // 2. LoRa incoming (radio)
    // 3. BLE incoming (defragmented Reticulum packets from phone)
    // 4. Timer deadline (protocol maintenance, announces)
    // 5. GNSS time candidate (until the calendar is seeded once)
    // 6. Host wall-time injection (#238 control envelope)
    // 7. Host telemetry target (#238 control envelope, #236)
    // 8. Host fixed position (#238 control envelope)
    // 9. Telemetry evaluation tick (only while a target is configured)
    loop {
        transport_stats.poll(&node);
        // Close a `[DROP]` rate-limit window that expired without a further
        // drop to carry its summary (#346): the silence after a storm is the
        // reading that summary is for, and no event arrives to trigger it.
        crate::events::flush_drop_summary(node.now_ms());
        heap_census.poll(&node, pn_engine.as_ref());
        // Clamped by the stats deadline so the line is still emitted on a
        // channel quiet enough that the node itself has nothing scheduled.
        let deadline = node
            .next_deadline()
            .map(Instant::from_millis)
            .unwrap_or(Instant::MAX)
            .min(transport_stats.deadline())
            .min(heap_census.deadline());
        // The propagation engine's own schedule (queued work, announce,
        // sync scheduler) rides the same timer arm.
        let deadline = match pn_engine.as_ref() {
            Some(pn) => deadline.min(Instant::from_millis(pn.next_deadline_ms(node.now_ms()))),
            None => deadline,
        };

        let gnss_time_candidate = async {
            #[cfg(feature = "gnss")]
            {
                if time_seed_gate.is_seeded() {
                    // Seeded for this boot: nothing left to wait for.
                    core::future::pending::<u64>().await
                } else {
                    match gnss_rx.as_mut() {
                        Some(rx) => loop {
                            let fix = rx.changed().await;
                            if let Some(unix) = time_seed_gate.offer(fix.unix_secs) {
                                break unix;
                            }
                        },
                        None => core::future::pending::<u64>().await,
                    }
                }
            }
            #[cfg(not(feature = "gnss"))]
            {
                core::future::pending::<u64>().await
            }
        };

        // The tick rate is also the retry rate for a report the radio
        // could not take: the policy re-arms until a send is confirmed.
        // With no target configured there is nothing to wake up for.
        let telemetry_tick = async {
            match reporter.as_ref() {
                Some(reporter) if !reporter.is_off() => Timer::after(TELEMETRY_TICK_INTERVAL).await,
                _ => core::future::pending::<()>().await,
            }
        };

        // The periodic announce (#376 item 2). Sleeps exactly to its own
        // deadline rather than polling: an idle board with telemetry off
        // wakes twice an hour for this, and a clockless one once a
        // minute until its clock arrives.
        let announce_tick = async {
            Timer::after(Duration::from_millis(
                announce_gate.periodic_wait_ms(node.now_ms()).max(1),
            ))
            .await
        };

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
            gnss_time_candidate,
            select4(
                serial.wall_time_rx.receive(),
                telemetry_target_rx.receive(),
                fixed_position_rx.receive(),
                serial.announce_rx.receive(),
            ),
            select(telemetry_tick, announce_tick),
        )
        .await;
        // Mirror each interface's is_online() into the core (#365): a
        // path over an offline interface is no path, so a report due
        // while a carrier is off (or peerless, for BLE) falls through
        // to a path request instead of a silent carrier-off drop. After
        // the await, not at the loop top: a `--set-media ble=off` lands
        // in another task while this one sleeps, and the arm below must
        // route with the state as it is NOW.
        node.set_interface_online(0, serial_iface.is_online());
        node.set_interface_online(1, lora_iface.is_online());
        node.set_interface_online(2, ble_iface.is_online());
        // The peer-count sibling of the online mirror (Codeberg #365):
        // marks BLE as a peer-link carrier the transport may
        // re-originate an unanswerable path request on. Serial and LoRa
        // mirror nothing — no "peers" at this layer — and stay at zero.
        node.set_interface_peer_count(2, ble_iface.peer_count());
        // The LoRa PHY's price, mirrored into the announce cap in the same
        // place and for the same reason as the online mirror above: a
        // reconfigure lands in another task while this one sleeps, and the
        // arms below must throttle against the PHY as it is NOW.
        announce_cap.sync(&mut node);
        // The calendar's own healing, mirrored here for the same reason as
        // the state above: it happens in code this task does not run. The
        // seeding arms below record the source they inject, but
        // `learn_emission_timebase` seats an anchor from an overheard
        // announce with nothing at this layer called, so a board healed
        // from traffic kept saying the birth state forever (Codeberg #398).
        // Reading the node is what keeps the banner honest without a
        // notification path per source.
        if crate::set_time_source(node.time_source()) {
            log_critical!("[TIME_SOURCE] source={}", crate::time_source_str());
        }
        match wake {
            Either4::Second(unix) => {
                // A GNSS fix carrying UTC. The seam applies the same
                // sanity window as every other time source; a refusal is
                // surfaced as a structured event, never swallowed.
                #[cfg(feature = "gnss")]
                {
                    use leviculum_core::transport::TimeSource;
                    if node.set_wall_time_unix_secs(unix, TimeSource::Gnss) {
                        time_seed_gate.mark_seeded();
                        crate::set_time_source(TimeSource::Gnss);
                        log_critical!("[TIME_SEED] source=gnss unix={}", unix);
                        log_critical!("[TIME_SOURCE] source={}", crate::time_source_str());
                    } else {
                        log_critical!("[TIME_SEED_REFUSED] source=gnss unix={}", unix);
                    }
                }
                #[cfg(not(feature = "gnss"))]
                let _ = unix;
            }
            Either4::Third(Either4::First(unix_secs)) => {
                // A host that knows wall time (#238 TYPE_WALL_TIME). The
                // seam applies the same sanity window as every other time
                // source; the bool picks the enveloped ack or the named
                // refusal — mirror of the GNSS path.
                use leviculum_core::envelope;
                use leviculum_core::transport::TimeSource;
                let answer = if node.set_wall_time_unix_secs(unix_secs, TimeSource::Host) {
                    crate::set_time_source(TimeSource::Host);
                    log_critical!("[TIME_SEED] source=host unix={}", unix_secs);
                    log_critical!("[TIME_SOURCE] source={}", crate::time_source_str());
                    envelope::encode_ack(envelope::TYPE_WALL_TIME)
                } else {
                    log_critical!("[TIME_SEED_REFUSED] source=host unix={}", unix_secs);
                    envelope::encode_refusal(envelope::TYPE_WALL_TIME, envelope::REFUSE_VALUE)
                };
                // Best effort: a full outgoing channel means a busy link;
                // the host's retry covers it.
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::Third(Either4::Second(wire)) => {
                // A host set or cleared the telemetry target (#236). What
                // happens here is the part that needs the node — the
                // identity lookup that decides ready vs awaiting-key. The
                // persist and the answer both belong to the serial task
                // now: the answer may not go out before the record is on
                // the page (#358), and only the task that requested the
                // save can wait for it.
                use crate::telemetry::TargetOutcome;
                if let Some(reporter) = reporter.as_mut() {
                    match reporter.apply_target(&mut node, wire) {
                        TargetOutcome::Set(_) | TargetOutcome::Cleared => {
                            reporter.log_banner();
                        }
                    }
                }
            }
            Either4::Third(Either4::Third(position)) => {
                // A host set or cleared the fixed position. This is the
                // part that needs the reporter — the source switch and the
                // confirmation re-arm; the persist is the serial task's,
                // for the reason above.
                if let Some(reporter) = reporter.as_mut() {
                    reporter.apply_fixed_position(position);
                    match position {
                        Some(p) => log_critical!(
                            "[TELEMETRY] fixed-position set lat_e6={} lon_e6={} alt_e2={} alt_present={}",
                            p.latitude_e6,
                            p.longitude_e6,
                            p.altitude_e2.unwrap_or(0),
                            p.altitude_e2.is_some() as u8
                        ),
                        None => log_critical!("[TELEMETRY] fixed-position cleared"),
                    }
                }
            }
            Either4::Third(Either4::Fourth(())) => {
                // A host asked for an announce now (#376 `TYPE_ANNOUNCE`,
                // `lnflash --announce`, periculum's `announce_board`):
                // every announce this board makes on its own cadence,
                // made now.
                //
                // TWO announces, not one, because a board running the
                // propagation role has two destinations a peer can look
                // up: the `lxmf.delivery` one the telemetry path
                // announces, and the `lxmf.propagation` one the role
                // announces (`crate::pn::Engine::announce_now`).
                // A client that wants to upload needs the SECOND —
                // `Identity.recall` is keyed by destination hash, so
                // knowing the delivery destination tells it nothing
                // about the mailbox — and until this arm sent it, the
                // only way to learn it was to wait out the role's own
                // 300 s interval.
                //
                // Each half keeps its own rule: the delivery announce
                // stays clock-gated (an uptime-stamped announce poisons
                // the path ranking a desk measures), the role's is not
                // (#384 item 6 — a clockless board still announces, and
                // the contact it invites is what delivers the seed). So
                // the answer grades the COMMAND and not either half:
                // ack when at least one announce left the board, the
                // named refusal only when none did. Which one went out
                // is on the board's own `[ANNOUNCE] sent ... reason=`
                // lines — `pn-host` for the role, `host` for delivery.
                use leviculum_core::envelope;
                let pn_announced = match pn_engine.as_mut() {
                    Some(pn) => {
                        let out = pn.announce_now(&mut node);
                        if out.actions.is_empty() {
                            false
                        } else {
                            let mut ifaces: [&mut dyn Interface; 3] =
                                [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                            let dispatched =
                                dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                            crate::dispatch::settle(
                                "pn-announce-host",
                                &mut node,
                                &dispatched,
                            );
                            true
                        }
                    }
                    None => false,
                };
                let answer = match delivery_hash.as_ref() {
                    None => {
                        // No delivery destination was registered this
                        // boot: nothing exists to announce, and neither a
                        // retry nor the clock can change that.
                        envelope::encode_refusal(
                            envelope::TYPE_ANNOUNCE,
                            envelope::REFUSE_UNSUPPORTED,
                        )
                    }
                    Some(_) if !node.has_plausible_wall_clock() => {
                        // The telemetry path's clock gate: the emission
                        // timestamp inside the announce is what peers
                        // rank paths by, and an uptime-stamped announce
                        // would poison the very path under measurement.
                        log_critical!("[ANNOUNCE] withheld reason=no-clock");
                        envelope::encode_refusal(envelope::TYPE_ANNOUNCE, envelope::REFUSE_NO_CLOCK)
                    }
                    Some(hash) => {
                        let app_data = crate::telemetry::announce_app_data(node.identity());
                        match node.announce_destination(hash, Some(&app_data)) {
                            Ok(out) => {
                                let mut ifaces: [&mut dyn Interface; 3] =
                                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                                let dispatched =
                                    dispatch_actions(&mut ifaces, out.actions, &ifac_configs);
                                crate::dispatch::settle(
                                    "announce-host",
                                    &mut node,
                                    &dispatched,
                                );
                                let dh = hash.as_bytes();
                                log_critical!(
                                    "[ANNOUNCE] sent dst={:02x}{:02x}{:02x}{:02x} reason=host",
                                    dh[0],
                                    dh[1],
                                    dh[2],
                                    dh[3]
                                );
                                envelope::encode_ack(envelope::TYPE_ANNOUNCE)
                            }
                            Err(_) => envelope::encode_refusal(
                                envelope::TYPE_ANNOUNCE,
                                envelope::REFUSE_BUSY,
                            ),
                        }
                    }
                };
                // The role's announce is on the air even where the
                // delivery half was withheld or does not exist, and a
                // refusal would report the command as having done
                // nothing.
                let answer = if pn_announced {
                    envelope::encode_ack(envelope::TYPE_ANNOUNCE)
                } else {
                    answer
                };
                // Best effort, like the wall-time answer: a full outgoing
                // channel means a busy link; the host's retry covers it.
                let _ = serial_ctl_tx.try_send(answer);
            }
            Either4::Fourth(Either::First(())) => {
                // Telemetry evaluation (#236). Everything decided here is
                // decided in the policy crate; this arm reads the board's
                // sensors, hands them over, and dispatches whatever came
                // back.
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    let (readings, has_fix) = readings::collect_readings(&node, sd);
                    let actions = reporter.tick(&mut node, now_ms, has_fix, &readings);
                    if !actions.is_empty() {
                        let mut ifaces: [&mut dyn Interface; 3] =
                            [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                        let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                        // The reporter settles first: it is the only caller
                        // that owns a cadence the dispatch's verdict decides
                        // (#344). `settle` counts and logs afterwards, as at
                        // every other site.
                        reporter.note_dispatch(&dispatched);
                        crate::dispatch::settle("telemetry", &mut node, &dispatched);
                    }
                }
            }
            Either4::Fourth(Either::Second(())) => {
                // The periodic announce (#376 item 2). Independent of
                // telemetry: a node that reports rarely, or has no target
                // at all, still has to be findable, and a phone that
                // missed the peer-up announce gets one from here. The gate
                // owns the deadline and the clock rule; this arm dispatches
                // whatever it hands back, which is nothing before the
                // deadline and nothing without a plausible clock.
                let actions = announce_gate.periodic(&mut node, delivery_hash.as_ref());
                if !actions.is_empty() {
                    let mut ifaces: [&mut dyn Interface; 3] =
                        [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                    let dispatched = dispatch_actions(&mut ifaces, actions, &ifac_configs);
                    crate::dispatch::settle("announce-periodic", &mut node, &dispatched);
                }
            }
            Either4::First(Either4::First(data)) => {
                info!("SER RX {} bytes", data.len());
                let output = node.handle_packet(InterfaceId(0), &data);
                info!("SER RX -> {} actions", output.actions.len());
                // An inbound TELEMETRY_REQUEST may ride any carrier (#371).
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                crate::dispatch::settle("ser-rx", &mut node, &dispatched);
                pn_step!(&output.events);
            }
            Either4::First(Either4::Second(data)) => {
                // A medium switched off at runtime stops carrying traffic
                // in BOTH directions from the moment the frame was
                // answered: the interface drops what the core hands it,
                // this drops what the medium hands up. Without the second
                // half a "LoRa off" node would still deliver over LoRa,
                // which is precisely the masking the profile exists to
                // remove.
                if !crate::media::lora_active() {
                    continue;
                }
                let output = node.handle_packet(InterfaceId(1), &data);
                if !output.actions.is_empty() {
                    info!("LORA RX -> {} actions", output.actions.len());
                }
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                crate::dispatch::settle("lora-rx", &mut node, &dispatched);
                pn_step!(&output.events);
            }
            Either4::First(Either4::Third(Either::First((peer, data)))) => {
                // #388 census: this packet just left BLE_INCOMING
                // custody (counted by the session that queued it).
                crate::ble::incoming_held_sub(data.capacity());
                // The reception itself is already on the log: columba's
                // `BLE: RX <n>B conn=<h> frags=<k>` line names the link
                // and the peer's fragmentation (#376), so a second
                // per-packet line here said less and doubled the noise.
                // See the LoRa arm: a medium switched off at runtime
                // delivers nothing upward either.
                if !crate::media::ble_active() {
                    continue;
                }
                // Name the ingress link when the handshake did (#365):
                // paths learned through it become attributable to the
                // peer when the link dies.
                let output = match peer {
                    Some(peer) => node.handle_packet_from_peer(InterfaceId(2), peer, &data),
                    None => node.handle_packet(InterfaceId(2), &data),
                };
                if !output.actions.is_empty() {
                    info!("BLE RX -> {} actions", output.actions.len());
                }
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                crate::dispatch::settle("ble-rx", &mut node, &dispatched);
                pn_step!(&output.events);
            }
            Either4::First(Either4::Third(Either::Second(event))) => {
                // A BLE peer transition (Codeberg #365). Lost: cull the
                // paths whose next hop is that peer so the next report
                // re-resolves over a carrier that can still deliver.
                // Up: pull the peer's delivery path over the fresh link
                // (Columba answers a path request but announces on
                // neither connect nor reconnect). Not gated on
                // media::ble_active — the cull is state cleanup, and a
                // pull for a medium switched off mid-run dies in the
                // interface's carrier-off drop like any other packet.
                let (label, output) = match event {
                    BlePeerEvent::Lost(peer) => {
                        let output = node.handle_interface_peer_lost(InterfaceId(2), peer);
                        info!(
                            "BLE peer lost, {} paths culled",
                            output
                                .events
                                .iter()
                                .filter(|e| matches!(e, NodeEvent::PathLost { .. }))
                                .count()
                        );
                        ("ble-peer-lost", output)
                    }
                    BlePeerEvent::Up(peer) => {
                        let mut output = node.handle_interface_peer_up(InterfaceId(2), peer);
                        info!("BLE peer up, {} pull actions", output.actions.len());
                        // #376: and announce OURSELVES to that peer, on
                        // its link alone. The peer just finished its
                        // identity handshake, so it can receive and it is
                        // exactly the node that does not know us; the pull
                        // above only asks what IT is. A broadcast here
                        // would reach the neighbour board, which forwards
                        // it, and the relayed copy racing the direct one
                        // is the two-hop reading this issue opened with.
                        output.actions.extend(announce_gate.peer_up(
                            &mut node,
                            delivery_hash.as_ref(),
                            peer,
                        ));
                        // And the propagation role (#384): the phone
                        // that just linked is exactly the client that
                        // should learn its board is a mailbox.
                        if let Some(pn) = pn_engine.as_mut() {
                            output
                                .actions
                                .extend(pn.on_ble_peer_up(&mut node, 2, peer).actions);
                        }
                        ("ble-peer-up", output)
                    }
                };
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                crate::dispatch::settle(label, &mut node, &dispatched);
                pn_step!(&output.events);
            }
            Either4::First(Either4::Fourth(())) => {
                let output = node.handle_timeout();
                if !output.actions.is_empty() {
                    info!("timeout: {} actions", output.actions.len());
                    if B::LOG_TIMEOUT_ACTIONS {
                        log_actions(&output.actions);
                    }
                }
                // A receipt timeout surfaces here, not on a reception —
                // the proof wait (#373) needs this arm's events too.
                if let Some(reporter) = reporter.as_mut() {
                    let now_ms = node.now_ms();
                    reporter.handle_inbound_events(&node, &output.events, now_ms);
                }
                let mut ifaces: [&mut dyn Interface; 3] =
                    [&mut serial_iface, &mut lora_iface, &mut ble_iface];
                let dispatched = dispatch_actions(&mut ifaces, output.actions, &ifac_configs);
                crate::dispatch::settle("timeout", &mut node, &dispatched);
                pn_step!(&output.events);
            }
        }
    }
}
