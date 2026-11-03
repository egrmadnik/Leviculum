# echo — LXMF loopback device

The smallest possible automation device: a remote node puts up to 32
arbitrary bytes into an `ECHC` params frame, the board stores them and
the next `ECHS` report sends the same bytes back. Send "ping", get
"ping" — the whole LXMF round trip exercised with a caller-chosen
payload.

```text
remote ──LXMF(content = ECHC: len + payload)──▶ App<Echo>::on_message → apply
remote ◀──LXMF(content = ECHS: len + payload + applied + uptime)── tick → report
```

## Why it exists

An echo node verifies every link in the chain end-to-end, each with its
own observable:

| Check | Observable |
|---|---|
| Path resolution | the params message arrives at all |
| Signature | `verification` field of `Inbound::Applied` |
| Frame codec | `ECHC` decodes — bad frames → `Rejected` |
| Target learning | reports go to the *sender*, no manual address |
| Report encode | remote unpacks + `verification` on the reply |
| Persistence path | `Inbound::Applied.blob` round-trips the store |
| Freshness | `applied` counter increments per params |

## Wire frames

### `ECHC` v1 — params, 33-byte payload

| Offset | Type | Field | Rule (`valid()`) |
|---|---|---|---|
| 0 | u8 | `len` | ≤ `ECHO_MAX` (32) |
| 1 | 32 B | `payload` | only `payload[..len]` is real |

`sample_ms` and `report_every` are not fields — the echo has no
tunables; `Params` reports `sample_ms = 1000`, `report_every = 1` as
constants so a reply leaves promptly.

### `ECHS` v1 — report, 41-byte payload

| Offset | Type | Field |
|---|---|---|
| 0 | u8 | `len` — echoed length |
| 1 | 32 B | `payload` — the echoed bytes |
| 33 | u32 LE | `applied` — params applied since boot (boot default counts) |
| 37 | u32 LE | `uptime_s` |

## The device has no I/O

```rust
type Input = ();
type Output = ();
```

It ticks against `NoIo` — `sample()` yields `()`, `drive()` is a no-op.
The cadence exists only to produce reports. `apply()` stores the payload
and bumps `applied`; `step()` returns `()`; `report()` copies the
stored payload into `ECHS`.

## Host simulation — `examples/echo_node.rs`

```sh
cargo run -p leviculum-automation --example echo_node
```

```text
[echo] board=776c3d51… remote=e03d0357…
== remote sends "ping" ==
[AUTO] params=applied from=e03d0357 verified=true
[echo] remote got "ping" applied=2 verified=Valid
== remote sends "hello leviculum" ==
[echo] remote got "hello leviculum" applied=3 verified=Valid
```

Both directions verify: the board sees `Verification::Valid` on the
params (it was given the remote's identity as `signer`), the remote sees
`Verification::Valid` on the report (it knows the board's identity).

## Firmware — `auto_echo` (nRF52840 xiaokit)

`leviculum-nrf/src/bin/auto_echo.rs` puts `App<Echo>` on the real board:

```sh
cargo build --release --features bsp-xiaokit --bin auto_echo   # v leviculum-nrf/
# UF2:
tools/uf2-runner.sh target/thumbv7em-none-eabihf/release/auto_echo
# ali ročno: cp .../auto_echo.uf2 /media/$USER/XIAO52840/
```

Differences from `lxmf_echo` (the raw-LXMF echo):

- no `LxmfNode` inbox — the app unpacks the opportunistic packet itself,
  so the delivery destination registers *without* an inbox
- echo logic lives in `leviculum_automation::echo`, not in the binary
- the params blob is a RAM store — `ECHC` is data to echo, not config
  worth flash wear (and on a SoftDevice build loop NVMC writes are
  illegal anyway)

Serial output on delivery:

```text
[ECHO] delivery_hash=1ef5d2e9a58ef444f16740b806aa2080
[AUTO] params=applied from=e03d0357 verified=false
[ECHO] reply sent dest=e03d0357
```

## Failure modes

| Symptom | Meaning |
|---|---|
| `params rejected` | `ECHC` frame corrupt or `len > 32` |
| `reply failed (no path)` | target learned but path gone before the report left |
| `applied` jumps >1 per message | board rebooted (boot-default apply counts) |
| report with empty `len` | no params received yet — boot default |
