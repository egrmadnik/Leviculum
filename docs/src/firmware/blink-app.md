# The Blink app

The Blink app turns the blue LED of a XIAO kit into something a remote
node controls over LXMF. Send the text `blink 2` and the LED flashes at
2 Hz; the board answers `blink set: 2.00 Hz (period 500 ms)`, and after
the next reset it still flashes at 2 Hz. It is the smallest complete
example of the pattern [the automation app guide](automation-app-guide.md)
teaches: a command in, a hardware task, a persisted setting, an
acknowledgement out.

Build it with the `lxmf-blink` cargo feature:

```sh
cargo build -p leviculum-nrf --release \
    --features bsp-xiaokit,lxmf-blink --bin xiaokit
```

`lxmf-blink` and `lxmf-echo` are alternative apps; enabling both fails the
build. Without either, the `xiaokit` binary is the plain LNode.

## What it does

| Remote node sends | Board does | Board replies |
| --- | --- | --- |
| `blink 2` | blinks at 2 Hz | `blink set: 2.00 Hz (period 500 ms)` |
| `blink 0.5` | blinks every 2 s | `blink set: 0.50 Hz (period 2000 ms)` |
| `blink 0` | LED off | `blink set: off` |
| `blink 20` | nothing | `blink error: out of range 0.1-10 Hz, or 0 for off; still 2.00 Hz` |
| `blink abc` | nothing | `blink error: not a number; still 2.00 Hz` |
| anything else | nothing | `blink error: expected: blink <Hz>; still 2.00 Hz` |

The **blink frequency** is in hertz, 0.1 to 10, or `0` for off. Fractions
are fine. The keyword is case-insensitive and extra spaces are ignored. A
rejected command changes nothing: the previous frequency stays in force and
the reply says what it is.

Any sender whose LXMF signature verifies can change the frequency. There is
no allow-list; the worst a stranger can do is make an LED blink. An app that
drives something that matters needs one (see the
[automation app guide](automation-app-guide.md#safety-checklist)).

After a reset the board starts with the stored frequency. If nothing valid
is stored (a new board, a blank or corrupt record) it starts at 1 Hz.

## How it is put together

```text
                      main loop (cooperative, owns the LXMF node)
 remote node ──LXMF──▶ Responder::on_events ──▶ blink::reply(text)
 remote node ◀──LXMF── (queue / path request)       │  parse, validate
                                                    ├─▶ CURRENT   (AtomicU16)
                                                    ├─▶ TARGET ───────────────┐
                                                    └─▶ SAVE ──────────┐      │
                                                                       ▼      ▼
                     Embassy tasks                          persist_task   led_task
                                                                  │          │
                                         telemetry store task ◀───┘          └─▶ blue LED (P0.06)
                                                  │
                                                  ▼
                                      flash page 0xEA000, record `LBLK`
```

Three pieces, each with one job:

- **The responder** ([`node/responder.rs`](../../../leviculum-nrf/src/node/responder.rs))
  is the LXMF side. It registers a full `lxmf.delivery` inbox, feeds every
  core event to the LXMF node, and for each received message calls the app's
  reply function with the message text and sends back whatever it returns.
  It does not know what blinking is.
- **The app** ([`node/blink.rs`](../../../leviculum-nrf/src/node/blink.rs))
  parses commands, owns the LED and decides what is saved.
- **The board** supplies the pin: the `Board::blink_led` hook, implemented
  in [`bin/xiaokit.rs`](../../../leviculum-nrf/src/bin/xiaokit.rs), returns
  the blue LED (P0.06, active low) as an output in its dark state.

### The responder

`Responder::register(node, title, reply)` registers the delivery destination
under the node's own identity, so the destination hash is the same as in the
plain build, and keeps `title` (the LXMF title of every reply) and
`reply: fn(&[u8]) -> Vec<u8>`.

`on_events` runs for every batch of core events, in the same place the
propagation engine does. For each event it asks the LXMF node to handle it,
dispatches whatever the LXMF layer wants on the wire (proofs, link setup),
and reacts to three LXMF events:

- `MessageReceived`: call `reply`, then send the result.
- `PeerAnnounced` and `DirectLinkEstablished`: try again the replies that
  could not be sent yet.

Sending a reply picks a direct link if one is open to the sender and an
opportunistic single packet otherwise. If the send fails for want of a path
or a key, the reply waits in a queue and a path request goes out; the queue
is retried when the peer announces or a link comes up. A permanent error (a
message that cannot be built, one too large) drops the reply.

`reply` runs in the main loop, once per message. It must not block and must
not wait for hardware.

### The LED task

```rust
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>, initial: u16) {
    let mut centi = initial;
    let mut lit = false;
    loop {
        if centi == 0 {
            led.set_level(Level::High);
            lit = false;
            centi = TARGET.wait().await;
            continue;
        }
        lit = !lit;
        led.set_level(if lit { Level::Low } else { Level::High });
        // Half a period: 1000 ms / hz / 2 = 50 000 / centi-hertz.
        let half = Duration::from_millis(50_000 / centi as u64);
        if let Either::First(new) = select(TARGET.wait(), Timer::after(half)).await {
            // Restart the phase on a change so it takes effect at once.
            centi = new;
            lit = false;
        }
    }
}
```

The task sleeps half a period, toggles, and sleeps again. The sleep is a
`select` between the timer and `TARGET`, a `Signal<_, u16>`: a new
frequency wakes the task immediately instead of at the end of the current
half period. A `Signal` holds only the latest value, which is what is wanted
here; ten commands in a row leave the LED at the tenth. At `0` the task
parks on `TARGET.wait()` with the LED dark and uses no timer at all.

Frequencies are kept in **hundredths of a hertz** (`u16`), so `0.5 Hz` is
`50` and the whole range fits in `0..=1000`. No floating point reaches the
task.

The LED is active low: `Level::Low` is lit. The HardFault handler drives
the same pin straight from the registers, so a fault overrides the blink.
That is intended: the blue LED is the fault light on this board.

### The command

```rust
fn parse(content: &[u8]) -> Result<u16, &'static str> {
    let text = core::str::from_utf8(content).map_err(|_| "not text")?;
    let mut words = text.split_whitespace();
    match words.next() {
        Some(w) if w.eq_ignore_ascii_case("blink") => {}
        _ => return Err("expected: blink <Hz>"),
    }
    let value = words.next().ok_or("expected: blink <Hz>")?;
    if words.next().is_some() {
        return Err("expected: blink <Hz>");
    }
    let hz: f32 = value.parse().map_err(|_| "not a number")?;
    // Written so that NaN fails: every comparison with it is false.
    if !(0.0..=10.0).contains(&hz) {
        return Err("out of range 0.1-10 Hz, or 0 for off");
    }
    let centi = (hz * 100.0 + 0.5) as u16;
    if valid(centi) { Ok(centi) } else { Err("out of range 0.1-10 Hz, or 0 for off") }
}
```

`parse` validates completely before anything is applied. `reply` then
either applies and acknowledges, or reports the error and the unchanged
frequency:

```rust
pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut out = String::new();
    match parse(content) {
        Ok(centi) => {
            CURRENT.store(centi, Ordering::Relaxed);
            TARGET.signal(centi);   // the LED task
            SAVE.signal(centi);     // the persist task
            // ... "blink set: ..." text ...
        }
        Err(why) => {
            // "blink error: <why>; still <current> Hz"
        }
    }
    out.into_bytes()
}
```

Note what `reply` does and does not do. It signals two tasks and writes an
atomic; all three are non-blocking. It does not wait for the LED to change
or for the flash write. The acknowledgement therefore means *accepted and
applied to the LED task*, not *written to flash*. The flash outcome is
logged separately.

### Persistence

The frequency lives in flash so it survives a reset and a firmware update.
Two tasks cooperate:

1. `persist_task` waits on `SAVE`. When a value arrives it waits for
   five quiet seconds (a newer value restarts the wait), then asks the
   telemetry store task to write it. One flash write follows a burst of
   commands rather than one per command.
2. The telemetry store task
   ([`telemetry.rs`](../../../leviculum-nrf/src/telemetry.rs)) owns the
   persistence page at `BoardConfig::telemetry_flash_page` (0xEA000, the
   lowest page the bootloader never overwrites). The page holds several
   independent records; the blink record sits at offset `+0x600`:

   | Bytes | Content |
   | --- | --- |
   | 0..4 | magic `LBLK` |
   | 4..6 | frequency in hundredths of a hertz, little endian |
   | 6..8 | check word: the frequency XOR `0xB11C` |

   Flash erases a whole page, so the store task reads every record back
   and rewrites them all with the new one. If the page already holds the
   same bytes it skips the write and the erase. A save that fails is
   retried three times, 250 ms apart.

At boot, `blink::start` reads the record. Blank flash (all `0xFF`) fails
the magic; a torn write fails the check word; a value outside the valid
range is rejected. All three mean the 1 Hz default, logged as
`src=default`.

The debug port shows the cycle:

```text
[BLINK] start hz=1.00 src=default      boot, nothing stored
[BLINK] set hz=2.00                    a command was accepted
[TELEMETRY] persist saved blink        five quiet seconds later
[BLINK] saved hz=2.00
[BLINK] start hz=2.00 src=stored       after the next reset
```

## Where each part is wired in

| Concern | Where |
| --- | --- |
| Cargo feature | `lxmf-blink` in `leviculum-nrf/Cargo.toml` |
| Module and the "one app at a time" check | `node/mod.rs` |
| Start the tasks after the boot indicator | `node::run`, `blink::start(&spawner, led, B::CONFIG.telemetry_flash_page)` |
| Register the inbox instead of the announce-only destination | `node/mainloop.rs`, `Responder::register(.., blink::TITLE, blink::reply)` |
| Hand events to the app | `pn_step!` macro in `node/mainloop.rs` |
| The LED pin | `Board::blink_led` hook (`node/board.rs`), implemented in `bin/xiaokit.rs` |
| Flash record and save channel | `telemetry.rs`: `BLINK_OFFSET`, `load_blink`, `request_save_blink` |

## Trying it

Flash the UF2 (`target/xiaokit_blink.uf2`, made from the `xiaokit` ELF with
`bin2uf2 --base 0x27000 --family 0xADA52840`). The XIAO shows two serial
ports; the one with the `[BLINK]` lines is the debug port. Send commands
with any LXMF client addressed to the board's delivery hash (the `lxmf=`
value of the `[IDENTITY]` line). With the repository's own client:

```sh
lxmf-node --config leviculum-automation/tests/lnsd-config lxmf-term
# then:  wait_for_peer <hash> 90
#        send <hash> <base64 of "blink 2">
```

Expect the reply text from the table above and the log lines of the last
section. To check persistence, wait for `[BLINK] saved`, reset the board
and look for `src=stored`.

## Limits

- Any verified sender can change the frequency; there is no allow-list.
- The acknowledgement does not say the value is in flash. A power cut in
  the five seconds before the save loses the last change.
- A reply that cannot be sent (no path to the sender) waits in a queue that
  has no age limit and is lost at reset.
- The responder's log lines say `echo` for every reply; they are shared with
  the echo app.
