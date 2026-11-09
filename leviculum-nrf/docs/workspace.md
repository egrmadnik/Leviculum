# The workspace — 30 crates, one firmware

`leviculum-nrf` is excluded from the repo-root workspace (embedded
target, own `.cargo/config.toml` for `thumbv7em-none-eabihf` + 
flip-link) and is itself the root of a small workspace: the firmware
crate plus **29 pure member crates**. Each member is `no_std`-clean,
dependency-free, and host-`cargo test`-able — the host-test seam made
concrete.

```
leviculum-nrf/                     the firmware crate (src/, cross-compiled)
├── screen            leviculum-screen       status-screen painter
├── sd-policy         leviculum-sd-policy    SoftDevice ownership policy
├── gnss-time         leviculum-gnss-time    NMEA→unix time + seed-once
├── gnss-presence     leviculum-gnss-presence baud sweep + fix hysteresis
├── gnss-init         leviculum-gnss-init    GNSS bring-up sequencing
├── telemetry-policy  leviculum-telemetry-policy  telemetry cadence/target
├── announce-policy   leviculum-announce-policy   announce cadence + rate limit
├── media-state       leviculum-media-state  running vs configured media
├── channel-access    leviculum-channel-access    CSMA jitter + CAD gate
├── tx-spacing        leviculum-tx-spacing   gap between packet airtimes
├── rx-arming         leviculum-rx-arming    radio re-arm order
├── settle-budget     leviculum-settle-budget     PN settle slice
├── mute-lease        leviculum-mute-lease   host mute deadline
├── sync-batch        leviculum-sync-batch   peer-sync batch counters
├── upload-proof      leviculum-upload-proof upload receipt window
├── queue-budget      leviculum-queue-budget outbound LoRa admission
├── drop-budget       leviculum-drop-budget  per-second [DROP] log budget
├── ble-tx            leviculum-ble-tx       HVN queue flow control
├── battery-scale     leviculum-battery-scale SAADC→mV→1S/2S
├── boot-count        leviculum-boot-count   flash-persisted reset count
├── boot-trace        leviculum-boot-trace   noinit-RAM breadcrumbs
├── record-log        leviculum-record-log   forward-only record log
├── pn-store          leviculum-pn-store     propagation store adapters
├── store-spike       leviculum-store-spike  #384 size harness
├── persist-ack       leviculum-persist-ack  flash-write ack gate
├── log-line          leviculum-log-line     byte-exact debug line shape
├── qspi-bitbang      leviculum-qspi-bitbang hand-clocked JEDEC-id read
├── qspi-boot         leviculum-qspi-boot    QSPI boot timeout
└── qspi-selftest     leviculum-qspi-selftest external-flash self-test
```

## Why the seam exists, in the crate's own words

> This crate cannot run a host test — both BSP features route through
> `softdevice`, one of them is mandatory, and `nrf-softdevice-s140`
> does not compile for a host triple. Every `#[test]` in this workspace
> lives in one of the pure member crates beside it.
> (`src/lib.rs`)

So a *decision* — a cadence, a threshold, an ordering, a predicate, an
arithmetic budget, a byte-exact line other tools grep — belongs in a
member crate, and the firmware crate calls it. What stays in `src/` is
what takes a pin, a register, a SoftDevice syscall or an
`embassy_time::Instant`.

## The three kinds of member crate

**Pure policy tables** — `sd-policy` (error code + SD-enabled flag →
action), `telemetry-policy`, `announce-policy`, `media-state`,
`queue-budget`, `drop-budget`, `mute-lease`. Their bugs are in the
table, so the whole table is asserted on the host.

**Ordering / state machines** — `rx-arming` (the re-arm *precedes* the
hand-off), `persist-ack` (the frame answer vs the flash write),
`boot-trace` (phase names + RESETREAS decode). A bug in event order is
only provable against a scripted fake — which the firmware cannot run.

**Arithmetic with a fake clock or fake part** — `tx-spacing`,
`channel-access`, `settle-budget`, `battery-scale`, `qspi-bitbang`
(which edge launches a bit), `record-log` (power cut at every byte
offset), `boot-count` (power cut at every word boundary), `screen`
(golden frames).

## Who shares them

Several crates are shared verbatim with host-side tools, so the policy
a desk measures on a board is the policy the daemon runs:
`leviculum-announce-policy` is shared with `lnsd`;
`leviculum-log-line` shapes every capture consumer's grep target;
`leviculum-settle-budget` pins the airtime budget the frame tests
derive.

## Key points

- **The crate boundary is the test boundary.** If a fact can be
  asserted on a host, it lives in a member crate — the firmware crate
  is deliberately test-free.
- **29 crates, one decision each.** A member is small enough to hold
  its whole argument in one file and its whole proof in one test file.
- **Shared verbatim with hosts** where the same policy must run on the
  desk and the board.
- **`qspi-selftest` is optional and off** — it exists to drag the
  self-test instrument's code into size measurements, not to ship.