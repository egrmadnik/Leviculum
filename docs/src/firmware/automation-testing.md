# Testing an automation device

How to prove, stage by stage, that an automation device on an LNode
works — from host unit tests through a bench session on the debug port
to a live mesh with a remote host setting parameters and receiving
reports. Each stage depends only on the ones before it, so a failure
localises itself: if stage 3 passes and stage 4 does not, the problem is
the radio path, not the frames or the flash.

The worked device is the thermostat in `leviculum-automation` on the
XIAO ESP32-S3 + Wio-SX1262 kit (`xiao_s3_auto`). The host side is
`lnmsg/examples/thermostat_host.rs`.

## What you need

| Stage | Hardware |
|---|---|
| 0 — host tests | nothing |
| 1 — flash and boot | the XIAO ESP32-S3 kit, USB-C cable |
| 2 — bench (no radio) | + a DS18B20 on D1 with a 4.7 kΩ pull-up to 3V3; optionally an LED + resistor on D3 to see the actuator |
| 3 — mesh | + a second LNode with a transport serial link to the PC (a XIAO nRF52840 kit on `lnsd` is the tested setup), antennas on both |

Toolchain: the Xtensa toolchain from `espup` (`. ~/export-esp.sh`),
`espflash`, and the host workspace build. See
[Building and flashing](flashing.md).

## Stage 0 — host tests

Everything that is not a pin is tested on the host.

```bash
cargo test -p leviculum-automation
```

Expected: 13 tests, all passing. What they prove:

| Test | Claim |
|---|---|
| `frame::tests::rejections` | wrong magic, wrong version, flipped byte, truncation are each refused |
| `thermostat::tests::params_frame_roundtrip_and_validity` | `THRM` encodes/decodes; `sample_ms=10` and `NaN` setpoint are refused as `Invalid` |
| `thermostat::tests::report_frame_roundtrip` | `THRS` encodes/decodes, including the 24-bit uptime |
| `thermostat::tests::hysteresis_band` | on below band, off above, hold inside, off on no reading |
| `thermostat::tests::apply_resets_state` | a new tune does not inherit the old `on` |
| `app::tests::*` | boot from empty/corrupt/valid store; target round-trips; cadence; failsafe on a missing reading |
| `lxmf::tests::params_roundtrip_through_lxmf` | a signed LXMF message carrying `THRM` decodes to the same params and the sender's hash; `"hello"` content is `NotParams`; garbage is `BadFrame` |
| `lxmf::tests::report_is_a_signed_message` | the report message verifies `Valid` against the board identity |

If you add a device, add its three tests (params round-trip + validity,
report round-trip, the control rule) in the same shape; the frame and
LXMF tests are inherited.

The host example must build too:

```bash
cargo clippy -p lnmsg --example thermostat_host -- -D warnings
```

## Stage 1 — flash and boot

```bash
cd leviculum-esp && . ~/export-esp.sh
cargo build --release --bin xiao_s3_auto
espflash save-image --chip esp32s3 --flash-size 16mb \
    target/xtensa-esp32s3-none-elf/release/xiao_s3_auto xiao_s3_auto.bin
espflash flash --port /dev/ttyACM0 xiao_s3_auto.bin
```

If the board does not enter the bootloader, hold **BOOT**, tap
**RESET**, release BOOT, retry. Then open the port — any terminal at
any baud (USB Serial/JTAG ignores the rate):

```bash
espflash monitor --port /dev/ttyACM0
# or: picocom /dev/ttyACM0
```

Press RESET. The boot block to look for, in order:

```
[FW_BUILD] …
[BOARD] id=XS3 name=Seeed XIAO ESP32-S3 + Wio-SX1262
[AUTO] sensor=ds18b20 present=true        ← false if nothing on D1
[AUTO] dest=<32 hex>                       ← the board's LXMF address; keep it
[LORA] state=up                            ← down = radio init/configure failed
[AUTO] config=default                      ← first boot; `loaded` after a save
```

Checks:

- `present=false` with a sensor fitted → wiring: data on D1 (GPIO2),
  pull-up present, 3V3 not 5V.
- `[LORA] state=down` → the radio did not answer on SPI2. Stage 2 still
  works; stage 3 will not.
- `dest=` is printed every boot and stays the same across resets: the
  identity is in flash. If it changes, the identity sector write failed
  (look for it printing `identity=generated persisted=false` on the PID
  image; this image shares the sector).

## Stage 2 — bench, over the debug port

No mesh needed. The debug port takes three commands; each answers with
a `[AUTO]` line.

### 2a. Read the running parameters

```
AUTO GET
→ [AUTO] sp=21 hyst=0.5 sample_ms=10000 report_every=6
```

### 2b. See the regulator run

With a DS18B20 attached, every `report_every × sample_ms` (default
60 s) the board prints a report:

```
[AUTO] t=19.5 sp=21 on=true lost=false
```

Hold the sensor in your hand: `t` rises past `sp + hyst` and `on`
turns `false`. Let it cool: it turns `true` again below `sp − hyst`.
An LED on D3 follows `on`. The inside of the band holds the previous
state — that is the hysteresis, not a lag.

Pull the sensor: the next sample prints

```
[AUTO] input=none failsafe on=false
[AUTO] t=NaN sp=21 on=false lost=true
```

and the actuator is off until a reading returns. This is the failsafe
claim; it must hold on every device.

### 2c. Inject a parameters message as the radio would deliver it

The host example can produce the exact bytes `App::on_message`
consumes — a signed opportunistic LXMF message, content = `THRM` frame
— without a daemon:

```bash
cargo run -p lnmsg --example thermostat_host -- \
    --to <board dest from stage 1> --setpoint 23 --hysteresis 0.2 \
    --sample-ms 2000 --report-every 1 --bench-hex
```

It prints its own address and one line starting `AUTO LXMF …` (about
240 hex characters). Paste that whole line into the debug port. The
board answers:

```
[AUTO] params=applied from=c6eaed3e persisted=true
[AUTO] cmd=lxmf result=ok
```

Then `AUTO GET` shows the new values, and reports now come every
2 s. Note `from=` — it is the first four bytes of the host's address:
the board has recorded that host as its report target, which is how
stage 3's reports find their way back.

Negative checks — each must be refused, nothing applied:

| Paste | Expect |
|---|---|
| the line with one hex digit changed in the middle | `result=rejected` (CRC) or `not-lxmf` |
| `AUTO LXMF deadbeef` | `result=not-lxmf` |
| a `--bench-hex` line generated with `--sample-ms 100` | the host refuses it before printing: `parameters refused by ThermostatParams::valid` |

### 2d. Persistence

After 2c, press RESET. The boot block must now say

```
[AUTO] config=loaded target=true
```

and `AUTO GET` must return the injected values. Reflash the image
(`espflash flash` again) and check once more — config lives in a
sector above the app image and survives a reflash. To return to
defaults, erase that sector:

```bash
espflash erase-region --port /dev/ttyACM0 0x702000 0x1000
```

### 2e. Set the report target by hand

```
AUTO TARGET <32 hex of the host's address>
→ [AUTO] cmd=target result=ok persisted=true
```

Used when the host should receive reports before it has ever sent
parameters (a listen-only host).

## Stage 3 — mesh: a remote host sets parameters and receives reports

### 3a. The transport node and daemon

Have a second LNode (the nRF kit) on its transport serial port, and
`lnsd` running with a `SerialInterface` on it — the setup from
[Supported boards](boards.md). Verify the daemon sees the board:

```bash
lnsd -vv       # or journalctl -fu lnsd
```

Within ~60 s of the board booting, the path table grows by one and an
announce from `dest=` appears. `lnomad` lists it with the display name
`leviculum-xiao_s3-auto`. The board's debug port shows `[LORA] tx len=…`
for its announce and `[LORA] rx len=… rssi=… snr=…` for everything it
hears.

If nothing arrives on either side: both antennas fitted? Both on
869.463 MHz / SF8 / BW125 (the `eu_medium` profile the nRF board also
compiles in; a host-set radio profile on the nRF side must match)?
`[LORA] state=up` on the ESP?

### 3b. Run the host

```bash
cargo run -p lnmsg --example thermostat_host -- \
    --to <board dest> --setpoint 21.5 --hysteresis 0.3 \
    --sample-ms 10000 --report-every 3
```

Expected sequence on the host:

```
[host] our address c6eaed3e…  (the board will report to this)
[host] resolving 00b5bfe4…
[host] params queued id=…  (sp=21.5 hyst=0.3 sample=10000ms report_every=3)
[host] msg … → Sent
[host] waiting for reports (Ctrl-C to stop)
```

and, on the board's debug port, within one LoRa hop:

```
[LORA] rx len=… rssi=-62 snr=9
[AUTO] params=applied from=c6eaed3e persisted=true
```

Then every `3 × 10 s`, the board prints its report and `[LORA] tx`, and
the host prints:

```
[THRS] t=20.75 sp=21.50 on=true lost=false uptime=312s  from=00b5bfe4
```

`(unverified)` after a report means the host had not yet learned the
board's identity from an announce when the report arrived; it clears
once one is seen. A report that says `lost=true` is the board telling
you its sensor is gone.

### 3c. What a failure at each step means

| Symptom | Where to look |
|---|---|
| `resolving` never completes, times out | the board's announce is not reaching `lnsd`: stage 3a |
| `params queued` but no `params=applied` on the board | the packet did not cross the hop; check `[LORA] rx` on the board and `[LORA] tx` on the nRF side; try closer |
| `params=applied` but no `[THRS]` on the host | the board's reports are not reaching the host: is the host still attached (`lnsd` up)? does the board show `[LORA] tx` at report time? |
| `[AUTO] cmd=… result=rejected` on the board | the host built a frame the board's `Params::decode` refuses — version mismatch between host and board builds |
| `[other] … not-a-report` on the host | something else sent to the host's address; harmless |

### 3d. Persistence across the mesh

With reports flowing, press RESET on the board. It boots with
`config=loaded target=true`, re-announces, and the host's `[THRS]`
lines resume without anything being re-sent — the target survived.

Then stop and restart the host. It attaches with the same identity
(`${LNMSG_HOME}/identity`), so its address is unchanged and reports
keep arriving. A host started with `--listen-only` receives without
sending.

## Stage 4 — a different device or board

The same stages apply unchanged. For a new device: stage 0 is its three
unit tests; stage 2c needs a host that encodes its `Params` (copy
`thermostat_host.rs`, swap the two types). For the nRF kit: the adaptors
in `leviculum-nrf/src/automation.rs` and a bin following
`xiao_s3_auto`'s four-part shape; stages 1–3 then read identically with
the nRF's debug port in place of the ESP's.

## What is and is not proven

- Stages 0 and 2 are deterministic and have been run; the frames, the
  regulator, the flash blob and the LXMF codec are proven there.
- Stage 3 exercises `leviculum-esp/src/radio.rs` + `NodeCore` over the
  air. The driver is a straight port of the nRF one onto the same
  `sx126x` sequences, but it has **no CSMA** (a transmit can collide
  with an inbound frame) and polls DIO1 rather than taking an
  interrupt. A busy mesh will show that as occasional lost reports;
  a two-node bench will not.
- Inbound parameters are **not signature-checked** on the board (it has
  no copy of the host's identity). The host's reports *are* checked.
  Until an allow-list lands, anyone who can reach the board's address
  can retune it — a bench fact to know, not a surprise to meet.
