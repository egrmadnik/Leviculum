//! The cooperative event loop of an ESP LNode.
//!
//! One pass: serve the sensors, poll the radio, run the node's timers,
//! re-announce when due, feed the app the line typed on the debug port,
//! tick the app, then sleep 100 ms; every five seconds the board states
//! what it is again.

use leviculum_core::node::NodeEvent;

use crate::node::app::{App, Ctx};
use crate::node::boot::{self, BANNER_INTERVAL_MS};
use crate::node::peripherals::Sensors;

/// How often the node re-announces its destination so the fleet keeps our
/// path warm.
const REANNOUNCE_MS: u64 = 60_000;
/// First re-announce after boot.
const FIRST_REANNOUNCE_MS: u64 = 30_000;
/// Longest debug-port command line, bytes.
const LINE_MAX: usize = 128;

/// Run the node. Never returns.
pub async fn run<B: crate::node::board::Board, A: App>(
    mut ctx: Ctx<'_>,
    mut app: A,
    mut sensors: Sensors,
    #[cfg(feature = "display")] mut display: Option<crate::node::display::Display>,
    mut led: Option<esp_hal::gpio::Output<'static>>,
    led_active_high: bool,
) -> ! {
    let mut next_announce = FIRST_REANNOUNCE_MS;
    let mut next_banner = 0u64;
    let mut rx_line = [0u8; LINE_MAX];
    let mut rx_len = 0usize;

    loop {
        sensors.poll(ctx.log);

        // ---- mesh ingress ------------------------------------------------
        // Poll the radio, hand complete Reticulum packets to the node,
        // dispatch whatever comes back (rebroadcasts, our own traffic).
        // A WiFi build runs the node's timers even with the LoRa radio
        // down: the mesh may live on wire alone.
        if ctx.carrier.is_up() || cfg!(feature = "wifi") {
            if let Some(out) = ctx.carrier.ingest(ctx.node, ctx.log) {
                handle(&mut ctx, &mut app, out);
            }
            let out = ctx.node.handle_timeout();
            handle(&mut ctx, &mut app, out);

            if crate::uptime_ms() >= next_announce {
                announce(&mut ctx, &app);
                next_announce = crate::uptime_ms() + REANNOUNCE_MS;
            }
        }

        // ---- WiFi ingress -------------------------------------------------
        // Complete packets the socket tasks deframed, plus their log lines.
        #[cfg(feature = "wifi")]
        {
            while let Ok(line) = crate::node::wifi::LOG.try_receive() {
                ctx.log.line("[WIFI] ", format_args!("{line}"));
            }
            while let Ok((iface, packet)) = crate::node::wifi::INBOUND.try_receive() {
                let out = ctx.node.handle_packet(iface, &packet);
                handle(&mut ctx, &mut app, out);
            }
        }

        // ---- command ingress ---------------------------------------------
        // Lines typed on the debug port go to the app — except `WIFI …`,
        // which is the carrier's own console (`wifi::on_line` applies the
        // change to the running settings).
        let mut rx = [0u8; 64];
        let n = ctx.log.read_bytes(&mut rx);
        for &b in &rx[..n] {
            if b == b'\n' || b == b'\r' {
                if rx_len > 0 {
                    let line = core::str::from_utf8(&rx_line[..rx_len]).unwrap_or("");
                    #[cfg(feature = "wifi")]
                    if crate::node::wifi::on_line(line, ctx.log, ctx.flash) {
                        rx_len = 0;
                        continue;
                    }
                    app.on_line(&mut ctx, line);
                    rx_len = 0;
                }
            } else if rx_len < rx_line.len() {
                rx_line[rx_len] = b;
                rx_len += 1;
            } else {
                // Overrun — the line is garbage either way; drop it.
                rx_len = 0;
            }
        }

        #[cfg(feature = "display")]
        if let Some(display) = display.as_mut() {
            display.poll_button();
        }

        app.tick(&mut ctx, crate::uptime_ms());

        // Sleep: the executor runs the app's own tasks meanwhile.
        embassy_time::Timer::after_millis(A::TICK_MS).await;

        if crate::uptime_ms() >= next_banner {
            sensors.banner(ctx.log);
            #[cfg(feature = "display")]
            if let Some(display) = display.as_mut() {
                display.render(ctx.log);
            }
            if let Some(led) = led.as_mut() {
                boot::blink(led, led_active_high);
            }
            ctx.log.identify(B::CONFIG);
            next_banner = crate::uptime_ms() + BANNER_INTERVAL_MS as u64;
        }
    }
}

/// Events first (the app sees them before the actions leave), then the
/// actions onto the air.
fn handle<A: App>(ctx: &mut Ctx<'_>, app: &mut A, out: leviculum_core::transport::TickOutput) {
    let events: &[NodeEvent] = &out.events;
    app.on_events(ctx, events);
    ctx.dispatch(out);
}

/// Announce the app's destination with its display name.
pub fn announce<A: App>(ctx: &mut Ctx<'_>, app: &A) {
    if let Ok(out) = ctx
        .node
        .announce_destination(&app.delivery_hash(), Some(app.announce_data()))
    {
        ctx.dispatch(out);
    }
}
