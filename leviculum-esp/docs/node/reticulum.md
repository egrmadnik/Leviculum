# `node::reticulum` — identity and transport

`src/node/reticulum.rs`

The engine is the same `leviculum_core::node::NodeCore` the host daemon
and the nRF boards run, instantiated as

```rust
type Node = NodeCore<Trng, EspClock, MemoryStorage>;
```

- **`EspClock`** — `Clock` over `uptime_ms`, monotonic only;
  `wall_unix_secs` stays `None`. Announce emission timestamps come from
  the transport's learned timebase — the intended answer on a board
  with no RTC.
- **`identity(log, flash)`** — load the private key from its flash
  sector (`store::load_identity`), or generate it with the TRNG and save
  it. The destination hash a remote node addresses is only stable if
  the identity is. The caller must hold a `TrngSource` for the node's
  whole life — the TRNG only unlocks while one exists.
- **`build(identity)`** — `NodeCoreBuilder` with
  `enable_transport(true)`: the node forwards other nodes' traffic
  between its interfaces — with `wifi` that is the LoRa↔TCP/UDP bridge.
- **`register_interface(node)`** — names `InterfaceId(0)` `lora_sx1262`
  and sets HW_MTU 508 (the RNode figure the fleet agrees on).
