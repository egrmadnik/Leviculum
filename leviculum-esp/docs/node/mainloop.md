# `node::mainloop` — the cooperative event loop

`src/node/mainloop.rs`

One async loop, sleeping `App::TICK_MS` (default 100 ms) between
passes. Each pass, in order:

1. **sensors** — `sensors.poll(log)` (the BMV080 SDK wants its
   `serve_interrupt` at least once a second).
2. **mesh ingress** — if the carrier is up (or `wifi` is built, so the
   node's timers still matter with LoRa down): `carrier.ingest` hands
   complete LoRa packets to `node.handle_packet`, then
   `node.handle_timeout()` runs the transport's timers. Every
   `TickOutput` goes through `handle()`.
3. **re-announce** — first at 30 s, then every 60 s, so the fleet keeps
   our path warm (`announce` → `node.announce_destination` with the
   app's delivery hash and display name).
4. **WiFi ingress** (feature `wifi`) — drain `wifi::LOG` into `[WIFI]`
   lines, then each `(iface, packet)` in `wifi::INBOUND` through
   `node.handle_packet`.
5. **command ingress** — bytes from the debug port accumulate into a
   128-byte line buffer; a completed line goes to `app.on_line`.
   Overruns drop the line.
6. **app tick** — `app.tick(ctx, uptime_ms())`.
7. **sleep** — `Timer::after_millis(TICK_MS)`; the executor runs the
   app's and the WiFi tasks meanwhile.
8. **banner** — every 5 s: `sensors.banner` (one forced-mode BME690
   sample), a status-LED blink (unless the app owns it), and the
   `identify` line again.

## `handle` — events before actions

```rust
fn handle(ctx, app, out: TickOutput) {
    app.on_events(ctx, &out.events);  // the app sees deliveries/links first
    ctx.dispatch(out);                // then the actions go on the air
}
```

The app reacts to what arrived (an LXMF delivery, a link) before the
same batch's outbound packets are transmitted — so a reply computed in
`on_events` can still ride the batch's dispatch.
