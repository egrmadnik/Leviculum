# Writing an automation app for an LNode

This guide shows how to write a program that runs on an LNode board and
does a job in the physical world (regulate a temperature, switch a pump,
count pulses) while a remote node sets its parameters and reads its state
over LXMF. The running example is an HVAC regulator. The pattern is taken
from the [Blink app](blink-app.md), which is built, flashed and tested; the
HVAC code is illustrative and has **not** been compiled.

Two ways to write an automation program exist in this repository. This
guide teaches the node-level pattern: the app is a few modules inside
`leviculum-nrf`, uses plain-text commands and talks to hardware through
Embassy tasks. [Automation devices on an LNode](automation-device.md)
describes the portable `leviculum-automation` framework, with binary
`Params` and `Report` frames; see
[Which one to use](#which-one-to-use) at the end.

## The model

An automation app has four parts. Each runs in a different context, and
most mistakes come from putting code in the wrong one.

```text
 remote node ──LXMF text──▶ [1] command handler ──▶ settings (shared state)
 remote node ◀──LXMF text── [4] reply / report            │
                                                           ▼
                    sensor ──▶ [2] control task ◀── settings
                                      │
                                      ▼
                                  actuator          [3] persistence task
```

| # | Part | Runs in | Job |
| - | --- | --- | --- |
| 1 | **Command handler**: parses a text message, validates it, applies it, builds the reply | the main loop, inside the LXMF responder | the only code that talks LXMF |
| 2 | **Control task**: reads the sensor, decides, drives the actuator | its own Embassy task | the actual automation; must keep working whatever the radio does |
| 3 | **Persistence task**: writes settings to flash after they settle | its own Embassy task | survive resets without wearing flash |
| 4 | **Reply / report**: text going back to the remote node | the responder (replies); a task plus the responder (reports, see [below](#reports-nobody-asked-for)) | tell the operator what happened |

### The rules

1. **The LXMF node belongs to the main loop.** It cannot move into a
   task. A task that wants to say something to the remote node leaves a
   value in shared state and lets the main loop send it.
2. **The command handler never blocks.** It runs once per message inside
   the loop that also services the radios. It parses, validates, writes
   shared state and returns text. It does not wait for a sensor, an
   actuator or flash.
3. **Tasks never parse commands, and the handler never drives hardware.**
   The two sides meet only in shared state.
4. **The control task is independent of the link.** If no command ever
   arrives, or the radio is down, it keeps regulating with the last
   settings.
5. **Validate completely, then apply.** A rejected command changes
   nothing, and the reply says what is still in force.

## Recipe

The numbered steps below are what the Blink app does; each shows the real
code. Replace "blink" with your app's name.

### 1. One cargo feature, one module

Add a feature to `leviculum-nrf/Cargo.toml` and a module to `node/mod.rs`:

```toml
lxmf-blink = []
```

```rust
#[cfg(feature = "lxmf-blink")]
pub mod blink;
#[cfg(any(feature = "lxmf-echo", feature = "lxmf-blink"))]
pub mod responder;

#[cfg(all(feature = "lxmf-echo", feature = "lxmf-blink"))]
compile_error!("`lxmf-echo` and `lxmf-blink` are alternative apps: enable one");
```

An app is one cargo feature. There is one app per firmware image, and the
build refuses two.

### 2. Register the responder with your reply function

`node/mainloop.rs` registers the inbox. Your app supplies a LXMF title and
a function from received text to reply text:

```rust
#[cfg(feature = "lxmf-blink")]
let mut app = crate::node::responder::Responder::register(
    &mut node,
    crate::node::blink::TITLE,   // b"blink"
    crate::node::blink::reply,   // fn(&[u8]) -> Vec<u8>
);
```

The responder does the LXMF work: it registers the `lxmf.delivery`
destination under the node's identity (so the address does not change),
proves delivery, accepts direct links, queues a reply when there is no path
to the sender yet and retries when the peer announces. Your function sees
only bytes in and bytes out. For a new app, add a feature, a module and a
second `register` call here; the responder itself does not change.

### 3. Shared state: pick the right primitive

The handler and the tasks communicate through `static` items. Choose by
what the reader needs:

| Need | Use | Blink example |
| --- | --- | --- |
| The reader wants the *latest* value and wakes when it changes | `Signal<CriticalSectionRawMutex, T>` | `TARGET`: the new frequency for the LED task |
| The reader wants *every* value, in order | `Channel<_, T, N>` | (not needed; a regulator's setpoint is latest-wins) |
| Anyone may read the current value at any time, without waiting | `AtomicU16` and friends | `CURRENT`: the frequency, read for error replies |
| A struct of several settings that must change together | `Mutex<CriticalSectionRawMutex, RefCell<Settings>>` | (not needed for one value) |

A `Signal` that is signalled ten times before the reader runs delivers the
last value once. That is the right behaviour for a setting: the operator
who types `setpoint 20` then `setpoint 21` means 21.

### 4. The command handler

Parse, validate, apply, answer. This is the whole of Blink's handler:

```rust
pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut out = String::new();
    match parse(content) {
        Ok(centi) => {
            CURRENT.store(centi, Ordering::Relaxed);
            TARGET.signal(centi);   // wake the LED task
            SAVE.signal(centi);     // start the save timer
            let _ = write!(out, "blink set: {} Hz (period {} ms)", Hz(centi),
                           100_000 / centi as u32);
        }
        Err(why) => {
            let _ = write!(out, "blink error: {}; still {} Hz", why,
                           Hz(CURRENT.load(Ordering::Relaxed)));
        }
    }
    out.into_bytes()
}
```

Design the command language before writing the parser:

- **Plain text, one command per message**, `word value` or `word`. A
  person can type it from a phone client and a script can send it.
- **Integers for stored values.** Blink keeps hundredths of a hertz in a
  `u16`; an HVAC regulator keeps temperatures in hundredths of a degree in
  an `i16`. Floating point appears only in the parser, at the edge, and
  never reaches a task or flash.
- **Every number has a documented range**, and a value outside it is an
  error, not a clamp. Write the check so that NaN fails (`!(a..=b).contains(&x)`).
- **Every reply names the state afterwards**, success or failure. An
  operator who sees `error: ...; still 21.0 C` knows nothing changed.
- **Unknown input gets a usage line**, never silence.

### 5. The control task

The control task is an ordinary Embassy task. It owns its pins, reads
shared settings, and waits on a timer or on a settings change, whichever
comes first:

```rust
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>, initial: u16) {
    let mut centi = initial;
    loop {
        // ... act on `centi` ...
        let wait = Duration::from_millis(50_000 / centi as u64);
        if let Either::First(new) = select(TARGET.wait(), Timer::after(wait)).await {
            centi = new;            // a command arrived: act on it now
        }
    }
}
```

`select(SETTINGS.wait(), Timer::after(period))` is the idiom: the task
sleeps for its natural period and is woken early by a command. The task
that owns a pin gets it through a `Board` hook, so the pin number appears
in the board file only:

```rust
// node/board.rs
#[cfg(feature = "lxmf-blink")]
fn blink_led(&mut self) -> Option<embassy_nrf::gpio::Output<'static>> { None }

// bin/xiaokit.rs
#[cfg(feature = "lxmf-blink")]
fn blink_led(&mut self) -> Option<Output<'static>> {
    self.blue_pin.take().map(xiaokit::led_notification)
}
```

The default returns `None`, so a board without the hardware still
compiles and the app starts without the task. For an HVAC regulator the
hooks would be `hvac_sensor()` and `hvac_relay()`.

### 6. Persistence

A setting that is lost at reset is a bug in an automation device. Blink
shows the whole path:

1. **A record in the telemetry page.** The page at
   `BoardConfig::telemetry_flash_page` holds independent records at fixed
   offsets (`+0x000`, `+0x100`, ... `+0x600`). Add yours at the next free
   offset in `telemetry.rs`: a constant, an encode and decode function, a
   `load_*` function and a `request_save_*` function, a channel and a
   `PersistSlot`, and an arm in `store_task`. Blink's record is 8 bytes:
   magic `LBLK`, the value, and a check word. The compile-time assertion
   next to the offsets fails the build if two records overlap.
2. **A magic and a check on every record.** Erased flash (`0xFF`) fails
   the magic, a torn write fails the check. Both decode to `None`, and the
   app uses its default.
3. **Debounce.** A task waits until the setting has been quiet for a few
   seconds and saves once. Flash endures about 10 000 erases per page; an
   operator adjusting a setpoint in a series of commands must not spend them.

   ```rust
   #[embassy_executor::task]
   async fn persist_task() {
       loop {
           let mut value = SAVE.wait().await;
           while let Either::First(newer) =
               select(SAVE.wait(), Timer::after(SAVE_QUIET)).await {
               value = newer;
           }
           let save = crate::telemetry::request_save_blink(value);
           match crate::telemetry::confirm(save).await {
               leviculum_core::envelope::Persist::Durable => { /* log saved */ }
               _ => { /* log save FAILED */ }
           }
       }
   }
   ```
4. **Load and validate at boot**, before the control task starts:

   ```rust
   let (initial, source) = match crate::telemetry::load_blink(telemetry_page) {
       Some(c) if valid(c) => (c, "stored"),
       _ => (DEFAULT_CENTI_HZ, "default"),
   };
   ```

   A stored value is untrusted input like a command: it goes through the
   same range check. An app whose range changes in a later firmware must
   cope with an old record outside the new range.

The flash store task rewrites every record on every save, because flash
erases a whole page. It skips the write when nothing changed. You do not
manage that; you add a record and call `request_save_*`.

### 7. Start the tasks

`blink::start` loads the setting and spawns the tasks; `node::run` calls
it after the boot indicator, once the pin hook has produced the pin:

```rust
pub fn start(spawner: &Spawner, led: Output<'static>, telemetry_page: u32) {
    let (initial, source) = /* load, as above */;
    CURRENT.store(initial, Ordering::Relaxed);
    spawner.must_spawn(led_task(led, initial));
    spawner.must_spawn(persist_task());
}
```

## Worked example: an HVAC regulator (illustrative, not built)

A regulator keeps a room at a setpoint by switching a heating relay. It is
configured by commands, reports its state on request, and keeps its
settings across resets.

### Commands

| Command | Meaning | Range |
| --- | --- | --- |
| `setpoint <C>` | target temperature in degrees Celsius | 5.0 to 30.0 |
| `hyst <C>` | hysteresis, total band around the setpoint | 0.2 to 5.0 |
| `mode off\|heat` | `off` keeps the relay open | |
| `status` | no change; reply with the full state | |

Replies always end with the state that is now in force:

```text
> setpoint 21.5
setpoint set: 21.50 C | mode heat hyst 0.50 | now 20.8 C relay on
> setpoint 40
setpoint error: out of range 5-30 C | setpoint 21.50 C mode heat
> status
status: setpoint 21.50 C mode heat hyst 0.50 | now 20.8 C relay on | sensor ok
```

### Shared state

```rust
// All temperatures in hundredths of a degree Celsius.
#[derive(Clone, Copy, PartialEq)]
struct Settings { setpoint: i16, hyst: u16, heat: bool }

static SETTINGS: Signal<CriticalSectionRawMutex, Settings> = Signal::new();
static SAVE: Signal<CriticalSectionRawMutex, Settings> = Signal::new();
// For `status` and for error replies; written by the control task.
static NOW: AtomicI16 = AtomicI16::new(i16::MIN);        // i16::MIN = no reading
static RELAY_ON: AtomicBool = AtomicBool::new(false);
// The settings in force, for replies. Written only by the command handler.
static CURRENT: Mutex<CriticalSectionRawMutex, Cell<Settings>> = ...;
```

### The control task

The task regulates with hysteresis and a minimum switching time, and it
**fails safe**: with no valid sensor reading the relay opens.

```rust
const MIN_SWITCH: Duration = Duration::from_secs(120);   // protect the relay
const SENSOR_TIMEOUT: u8 = 3;                            // bad reads in a row

#[embassy_executor::task]
async fn control_task(mut relay: Output<'static>, mut sensor: impl Sensor,
                      mut s: Settings) {
    let mut on = false;
    let mut last_switch = Instant::now() - MIN_SWITCH;
    let mut bad = 0u8;
    loop {
        match sensor.read_centi_c().await {
            Ok(t) => { bad = 0; NOW.store(t, Ordering::Relaxed);
                       let half = (s.hyst / 2) as i16;
                       let want = s.heat && if on { t < s.setpoint + half }
                                            else   { t < s.setpoint - half };
                       if want != on && last_switch.elapsed() >= MIN_SWITCH {
                           on = want; last_switch = Instant::now();
                       }
                       if !s.heat { on = false; }       // `mode off` wins at once
            }
            Err(_) => { bad = bad.saturating_add(1);
                        if bad >= SENSOR_TIMEOUT {
                            NOW.store(i16::MIN, Ordering::Relaxed);
                            on = false;                  // fail safe
                        } }
        }
        relay.set_level(if on { Level::High } else { Level::Low });
        RELAY_ON.store(on, Ordering::Relaxed);
        if let Either::First(new) =
            select(SETTINGS.wait(), Timer::after(Duration::from_secs(10))).await {
            s = new;
        }
    }
}
```

Things to notice: a command takes effect within one loop pass because
`SETTINGS` wakes the select; the minimum switching time bounds relay wear
and does not apply to `mode off`, which is a safety action; the loop has
no `.await` that can wait forever except the sensor read, which the sensor
driver must bound.

### The command handler

```rust
pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut s = CURRENT.lock(|c| c.get());
    let text = match core::str::from_utf8(content) { Ok(t) => t, Err(_) => return err("not text", s) };
    let mut w = text.split_whitespace();
    match (w.next().map(|x| x.to_ascii_lowercase()).as_deref(), w.next(), w.next()) {
        (Some("setpoint"), Some(v), None) => match parse_c(v, 5.0, 30.0) {
            Some(c) => s.setpoint = c,
            None => return err("out of range 5-30 C", s),
        },
        (Some("hyst"), Some(v), None) => match parse_c(v, 0.2, 5.0) {
            Some(c) => s.hyst = c as u16,
            None => return err("out of range 0.2-5 C", s),
        },
        (Some("mode"), Some("off"), None) => s.heat = false,
        (Some("mode"), Some("heat"), None) => s.heat = true,
        (Some("status"), None, None) => return status(s),
        _ => return err("expected: setpoint <C> | hyst <C> | mode off|heat | status", s),
    }
    CURRENT.lock(|c| c.set(s));
    SETTINGS.signal(s);
    SAVE.signal(s);
    ok("set", s)
}
```

`ok`, `err` and `status` format the state line from `s`, `NOW` and
`RELAY_ON`. Every error path returns before anything is changed.

### Persistence

Add a record `LHVC` at the next free offset of the telemetry page: magic,
`setpoint` (i16), `hyst` (u16), `heat` (u8) and a check word. At boot,
`load_hvac(page)` returns `Option<Settings>`; any value outside the
ranges above is discarded for the safe default, `heat: false` and the
last known good setpoint of 20.00 C. A regulator that restarts in `off` is
a regulator the operator notices; a regulator that restarts heating
against a stale setpoint is not.

## Reports nobody asked for

What exists today is the reply: every command, including `status`, is
answered with the state. That covers an operator who polls.

A regulator also wants to report by itself: a periodic status, and an alarm
when the sensor fails. **That is not built.** The responder only answers
messages it receives; it has no send method of its own. Here is the design
that fits the existing code, to be implemented before a guide can call it
a recipe:

1. **A report target.** A stored delivery hash, set by a command
   (`report <hash>`), kept in a record like any other setting. Without a
   target the app reports nothing.
2. **A report task.** A task that wakes on a timer (say every ten
   minutes) and on events (sensor failed, relay state changed) and puts a
   `Report` into a depth-1 `Channel`. The task does not touch LXMF.
3. **A responder hook.** `Responder::send(dest, text)` exposes the send
   path the responder already has (direct link if open, otherwise
   opportunistic, queue and path request if not), and the main loop calls it
   when the channel holds a report. The main loop needs a new `select` arm
   for the channel, or a poll at its existing ticks.
4. **Reporting policy.** Alarms are sent immediately and repeated until the
   condition clears; periodic reports are rate-limited, because LoRa airtime is
   scarce. The telemetry reporter in `telemetry.rs` already implements a
   cadence and retry policy of this kind; reuse its shape.

## Safety checklist

An automation device acts on the physical world on the strength of a radio
message. Before deploying one:

- **Choose the failed state and make every failure go there.** For a
  heater: relay open. No sensor, no valid setting, a crashed task: open.
  Decide this per device and write it down.
- **Authenticate the commanders.** The responder verifies LXMF signatures,
  and the Blink app then accepts any verified sender. A regulator needs an
  allow-list of sender hashes, checked in the handler before anything is
  parsed; the responder passes you only the text today, so the check needs
  the source hash added to the reply function's inputs.
- **Bound every setting** in the parser and again where a stored value is
  loaded. Never rely on the sender's good manners.
- **Bound every wait.** A control task that can wait forever on a sensor
  has stopped regulating without saying so.
- **Rate-limit actuation.** Relays, compressors and valves have a minimum
  cycle time that no command may violate.
- **Do not let the radio starve the control loop.** The control task is
  its own Embassy task for this reason. Keep the main-loop handler short.
- **Test the failures**, not only the commands: unplug the sensor, send
  garbage, reset in the middle of a save.

## Testing

Everything on the LXMF side can be tested from a host with no extra
hardware, with the debug port open beside it.

1. Build with the app's feature, convert the ELF to a UF2 and flash it.
2. Open the debug serial port and look for the app's boot line
   (`[BLINK] start hz=1.00 src=default`).
3. Send commands with an LXMF client, e.g.
   `lxmf-node --config leviculum-automation/tests/lnsd-config lxmf-term`,
   then `wait_for_peer <hash> 90` and `send <hash> <base64 text>`. The
   board's delivery hash is the `lxmf=` value in the `[IDENTITY]` log line.
4. Check each case of the command table: a valid value, each range edge, a
   value just outside it, a malformed number, a wrong keyword, extra words.
   Check that the reply names the unchanged state after every error.
5. Wait for the save line, reset the board and check the stored value is
   loaded (`src=stored`).
6. Check the failures from the safety list.

## Which one to use

| | Node-level pattern (this guide) | `leviculum-automation` framework |
| --- | --- | --- |
| Command format | plain text, per app | binary `Params` frame with magic, version and CRC32 |
| Reports | replies; push reports need the design above | built in: status cadence and `Report` frames |
| Persistence | a record in the telemetry flash page | `ConfigStore` trait, one blob |
| Where it runs | inside `leviculum-nrf`, nRF52840 only | portable `no_std` crate, nRF52840 and ESP32-S3 wrappers |
| Best for | one board, a few settings, a human at the other end | a device that must run on several MCUs, a machine at the other end |

Use the node-level pattern when the remote end is a person with an LXMF
client and the settings are a handful of numbers, as with the Blink app.
Use the framework when the same regulator has to run on both MCU families
or the remote end is a program that wants a versioned binary protocol with
reports on a cadence; start from
[Automation devices on an LNode](automation-device.md).
