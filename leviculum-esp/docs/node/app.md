# `node::app` — the app seam

`src/node/app/mod.rs` + `pid_app.rs`, `echo.rs`, `blink.rs`

An LNode is a Reticulum transport *and* an application in one device:
the node (`NodeCore`) owns the mesh, the app owns the purpose. The app
answers the loop's questions through the `App` trait:

```rust
trait App {
    const OWNS_STATUS_LED: bool = false; // blink app: drives the LED itself
    const TICK_MS: u64 = 100;            // loop sleep between passes

    fn start(ctx: &mut Ctx, wiring: AppWiring) -> Self;
    fn delivery_hash(&self) -> DestinationHash;      // what we announce
    fn announce_data(&self) -> &'static [u8];        // display name
    fn on_events(&mut self, ctx, events: &[NodeEvent]) {}
    fn on_line(&mut self, ctx, line: &str) {}        // debug-port commands
    fn tick(&mut self, ctx, now_ms: u64) {}
}
```

The loop is async on the Embassy executor: `App` methods run in it and
**must not block**. An app that needs real concurrency spawns Embassy
tasks from `start` via `ctx.spawner` (Blink does this for the LED and
for flash persistence; the PID app for valve timing).

## The seam, in one picture

```
                         mainloop (owns node, carrier, log)
 ┌────────────────────────────────────────────────────────────────┐
 │  each pass the loop calls the app:                             │
 │    node events ──► app.on_events(ctx, events)                  │
 │    debug line  ──► app.on_line(ctx, line)                      │
 │    once/pass    ──► app.tick(ctx, now_ms)                      │
 │                                                               │
 │  …and the app answers:                                         │
 │    delivery_hash() · announce_data()   (what we announce)      │
 │    start(ctx, wiring) ──► register dest · spawn tasks          │
 │    ctx.dispatch(out) ──► actions to LoRa / WiFi                │
 └───────────▲───────────────────────────────────┬────────────────┘
             │ ctx.spawner                       │ on_events forwards
   ┌─────────┴─────────┐                ┌────────▼────────────────┐
   │ app's own tasks   │                │ responder (LXMF inbox)  │
   │ led · persist ·   │                │ title + reply(content)  │
   │ valve timing      │                │ → ack text, queued when │
   └───────────────────┘                │   no path yet           │
                                        └─────────────────────────┘
```

The app implements six methods; the loop calls them at fixed points;
`Ctx` is everything the app may touch for one call.

## `Ctx` — everything an app may touch

| field | what it is |
|---|---|
| `node` | the `NodeCore` transport (destinations, links, announces) |
| `carrier` | the LoRa carrier |
| `log` | the USB debug log |
| `flash` | the `SharedFlash` static |
| `identity` | the node's identity |
| `spawner` | task spawner for the app's own tasks |

`ctx.dispatch(out)` routes a `TickOutput` to the air: with `wifi`,
`node::wifi::egress` takes the WiFi-bound actions first, the LoRa
carrier takes what remains.

## How an app talks: LXMF through `responder`

Most apps should not touch Reticulum mechanics at all. `node::responder`
is a ready-made `lxmf.delivery` inbox: it registers the destination
under the node's identity, handles proofs, direct links, opportunistic
sends, path requests and reply queueing. The app supplies **two things**:

1. a title (`&'static [u8]`, e.g. `b"blink"`), and
2. a reply function `fn(&[u8]) -> Vec<u8>` — message content in,
   reply content out.

```rust
responder: Responder::register(ctx, TITLE, reply)?
```

Every LXMF message that arrives is passed to that function and its
return value is sent back to the sender — over a direct link when one
is open, opportunistically otherwise, queued behind a path request when
the peer is not yet reachable (see `responder.md`). That is the whole
protocol surface an app needs: **a command-and-reply function, pure and
testable**. The app code stays small because encryption, addressing,
retransmission policy and link management are all below the seam.

Apps that need more than replies can still drop to `ctx.node` directly —
the PID app does: it registers the delivery destination itself and reads
binary `PIDC` frames straight off `PacketReceived`, emitting signed
`PIDS` status messages without the responder's text-in/text-out shape.

## What an app can be

The seam is deliberately thin so the device role is just code. Examples
in the same class:

- **HVAC control** — room heating/cooling: a temperature sensor in, a
  valve or relay out, setpoints and PID tuning as LXMF commands (the
  shipped `pid_app` is exactly this)
- **Building energy management** — load shedding, duty cycling, tariff-
  aware scheduling, reporting consumption to a concentrator
- **Metering** — pulse counting (water/gas/electricity), periodic or
  on-demand meter reads over LXMF
- **Environmental monitoring** — particulate/CO₂/temperature sensors
  reporting to a collector (the BMV080/BME690 plumbing already exists)
- **Irrigation** — soil moisture in, valves out, schedule pushed by LXMF
- **Lighting / signalling** — remote indicators and beacons (`blink`)
- **Access / actuation** — door strikes, gates, pump run/stop with
  acknowledgements
- **Presence / alarm nodes** — event-driven messages on triggers, with
  LXMF ack as the audit trail

All of these are "sensor(s) + actuator(s) + a reply function" — the
mesh, the identity, the announce cadence and the retry policy are
inherited unchanged.

## The three applications

Selected by feature; `node::DefaultApp` resolves to exactly one.
`lxmf-echo` and `lxmf-blink` are alternatives — enabling both is a
`compile_error!`.

| app | feature | behaviour |
|---|---|---|
| `pid_app::PidApp` | *(default)* | the PID valve regulator: DS18B20 1-Wire temperature input, valve drive pin, setpoint/PID config persisted to flash, signed `PIDS` status reports over LXMF |
| `echo::EchoApp` | `lxmf-echo` | an `lxmf.delivery` inbox that replies `echo: <text>` — a `Responder` with nothing but a title and a reply function |
| `blink::BlinkApp` | `lxmf-blink` | the LED blinker below |

## Walkthrough: `BlinkApp`

`src/node/app/blink.rs` — the smallest app that still shows every seam.
Its contract: a remote node sends the LXMF text `blink <Hz>`; the LED
flashes at that frequency, the change is acknowledged by reply and
persisted to flash.

### Shape

Three pieces, sized to what must not block:

```
LXMF message ──► responder ──► reply() ──► TARGET ──► led_task   (flashes)
                       │           └────► SAVE ────► persist_task (flash)
                       └────────────────► acknowledgement back to sender
```

- `led_task` owns the LED pin (the app owns the status LED:
  `OWNS_STATUS_LED = true`, so the banner stops blinking it and `run`
  hands the pin over in `AppWiring::led`). It `select`s a half-period
  timer against `TARGET.wait()` — a new frequency restarts the phase
  immediately.
- `persist_task` waits for `SAVE`, then debounces: it keeps waiting
  while new values arrive, and only writes flash after `SAVE_QUIET`
  (5 s) of stillness — one write for a burst of commands.
- `reply` is the responder's reply function: runs **in the main loop**,
  parses the command, signals both tasks, and returns the
  acknowledgement text.

The LXMF node cannot move into a task (`Ctx` borrows the loop's `Node`),
so only the LED and the flash write are tasks — the protocol step stays
in the loop where `node` lives.

### What crosses the seams

Everything the pieces share is a `static` — Embassy primitives, no
locks held across waits:

| static | type | carries |
|---|---|---|
| `TARGET` | `Signal<u16>` | latest frequency to the LED task (newest wins) |
| `SAVE` | `Signal<u16>` | frequency to the persist task |
| `CURRENT` | `AtomicU16` | the in-force frequency, for replies |
| `LOG` | `Channel<LogEvent>` | lines the tasks want printed — they cannot reach `ctx.log`, so `tick` drains it |

Frequencies travel as `u16` hundredths of a hertz (`Hz` formats them as
`0.50`/`10.00`) — no floats in the protocol.

### Boot

`start` reads the stored `LBLK` record (`ctx.with_flash(store::load_blink)`;
a blank/corrupt/out-of-range record falls back to 1 Hz), logs
`[BLINK] start hz=… src=stored|default`, spawns the two tasks with
`ctx.spawner`, and registers the responder. From that point the app is
just `on_events → responder` and `tick → drain LOG`.

### The command

`parse` is strict: exactly `blink <Hz>` (case-insensitive verb, one
number, nothing else), `0`–`10 Hz` or `0` for off — written so `NaN`
fails every comparison. `reply` maps the result:

- good parse → update `CURRENT`, signal `TARGET` and `SAVE`, reply
  `blink set: 0.50 Hz (period 2000 ms)`
- bad parse → reply `blink error: <why>; still 1.00 Hz` — the current
  value is restated so a remote operator always learns the truth

That is the whole app: ~50 lines of logic (`parse`, `reply`), two small
tasks, and the responder owns the rest of LXMF.

## Key points

- **Apps are tiny by construction.** The mesh, identity, announce
  cadence and retry policy are inherited; the app adds sensor +
  actuator + a reply function.
- **The protocol step stays in the loop.** The LXMF node borrows the
  loop's `Node`, so it cannot move into a task — only hardware-driven
  work (LED, flash, valve timing) is spawned.
- **Tasks talk back through statics** (`Signal`/`Channel`/`Atomic`),
  never through `Ctx` — no locks held across awaits.
- **Every app announces its own destination** with a display name, so
  the fleet sees it in `rnpath`/`rnstatus`.
