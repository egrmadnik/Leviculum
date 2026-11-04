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
tunables; `Params` reports `sample_ms = 1000`, `report_every = 0`
(report on-demand when params are applied, no periodic reports when idle).

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
  (`accepts_links = false`, `ProofStrategy::None`, because the `ECHS`
  report itself is the reply)
- sender signature verification via
  `lxmf::peek_source(data).and_then(|src| node.storage().get_identity(&src))`,
  with `pinned_identities` retaining nearby (`hops <= 2`) and active
  `app.target()` identities so public mesh announces cannot evict the
  sender from the 8-slot `EmbeddedStorage::known_identities` table
- echo logic lives in `leviculum_automation::echo`, not in the binary
- the params blob is a RAM store — `ECHC` is data to echo, not config
  worth flash wear (and on a SoftDevice build loop NVMC writes are
  illegal anyway)
- if the return path to the sender is not yet in the path table when the
  `ECHS` report is ready, `auto_echo` queues `pending_report` and calls
  `node.request_path(&target_hash)` so the reply is sent as soon as the
  path resolves

Serial output on delivery:

```text
[ECHO] delivery_hash=85c390a37c34762a3af3b2024aa63fed
[AUTO] params=applied from=e0a5a22a verified=true
[ECHO] reply sent dest=e0a5a22a
```

## Live hardware test — `tests/echo-term.py`

`leviculum-automation/tests/echo-term.py` tests the full round trip from
a host running `lnsd` (either the system `lnsd` service on
`instance_name = default` or `./run-lnsd.sh`) via `lxmf-node`:

```sh
cd leviculum-automation/tests
# Enkratno pošiljanje (one-shot):
python3 echo-term.py 85c390a37c34762a3af3b2024aa63fed "ojla"

# Interaktivni terminal (brez besedila v argumentu — pošilja ob vsakem <Enter>):
python3 echo-term.py 85c390a37c34762a3af3b2024aa63fed
```

```text
Čakam na XIAO announce (resetirajte če treba) ...
  peer najden
→ ECHC b'ojla'  (42 B frame)
← ECHS b'ojla'  applied=5 uptime=878s
   echo OK
```

How `echo-term.py` works:
1. Spawns `lxmf-node --config lnsd-config echo-term` and waits for
   `EVENT lxmf_ready`.
2. Sends `announce` and `wait_for_peer <delivery_hash> 60` (which
   immediately emits a Reticulum `PATH_REQUEST` for `<delivery_hash>` and
   re-announces every 5 s so a freshly reset XIAO seeds its wall clock
   and learns the return route to the host).
3. Encodes the payload into a 42-byte `ECHC` frame and sends it as a
   single-packet opportunistic LXMF message via
   `send_opportunistic <delivery_hash> <b64>`.
4. Waits for `EVENT lxmf_msg_received`, base64-decodes `body_b64`, and
   verifies the 50-byte `ECHS` frame (`content`, `applied`, `uptime_s`).

## Failure modes

| Symptom | Meaning |
|---|---|
| `params rejected` | `ECHC` frame corrupt or `len > 32` |
| `reply deferred (requesting path)` | target path missing; `node.remove_path` + `node.request_path` issued and reply queued until path resolves |
| `applied` jumps >1 per message | board rebooted (boot-default apply counts) |
| report with empty `len` | no params received yet — boot default |

For a complete step-by-step protocol trace and annotated XIAO debug log, see [message-flow.md](message-flow.md).
