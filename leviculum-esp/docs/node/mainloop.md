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

## One pass, as a sequence

```
pass N                                  pass N+1
┌──────────────────────────────────────────────┐
│ 1. sensors.poll                              │
│ 2. carrier.ingest ──► node.handle_packet ─┐  │
│    node.handle_timeout ───────────────────┤  │
│    (re-announce every 60 s)               │  │
│ 3. wifi: drain LOG, INBOUND ──► node ─────┼──┤
│ 4. commands: read bytes ──► app.on_line   │  │
│    (WIFI … → wifi::on_line instead)       │  │
│ 5. app.tick                               │  │
│ 6. sleep TICK_MS (100 ms) ──► executor    │  │
│    runs the app's and wifi's tasks        │  │
│ 7. every 5 s: banner + LED blink          │  │
└──────────────────────────────────────────────┘
```

Every step that returns a `TickOutput` funnels through `handle()`:
the app sees the events, then the actions go on the air.

## Key points

- **One loop, all ingress.** LoRa, WiFi and the debug port are three
  polling points in the same pass — no locks, no queues except the
  bounded channels the tasks feed.
- **The app never blocks.** `on_events`/`on_line`/`tick` run in the
  loop; anything slow or timed must be a spawned task.
- **The node's timers run even with LoRa down** on a `wifi` build —
  the mesh may live on wire alone.
- **Announce cadence is 60 s** (first at 30 s) so the fleet keeps our
  path warm.
