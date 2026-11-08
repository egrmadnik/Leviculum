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

## Identity and node, in one picture

```
                 flash sector 0x701000
                 ┌────────────────────┐
                 │ "IDT1" + key + crc │
                 └─────────┬──────────┘
                           │ load (or generate + save once)
                           ▼
   TrngSource (held for life) ──► Identity
                                   │
                                   ▼
   EspClock (uptime_ms) ──► NodeCoreBuilder
   MemoryStorage ────────►   .enable_transport(true)
   Trng (node entropy) ──►   .build_boxed() ──► Box<Node>
                                                   │
                        ┌──────────────────────────┤
                        ▼                          ▼
            register_interface(0)       wifi::register_interfaces
            "lora_sx1262" · MTU 508      "wifi_udp" · "wifi_tcp"
```

Why `Box<Node>`: the transport is a > 40 KiB struct — `build_boxed`
keeps it off the stack, on the heap.

## Key points

- **The identity is the address.** The destination hash a remote node
  sends to is derived from this key; it only stays stable because the
  key is persisted once and reused.
- **No RTC, no wall clock.** `EspClock` is monotonic; announce
  timestamps come from the transport's learned timebase.
- **Transport is on.** The node forwards other nodes' traffic between
  its interfaces — with `wifi`, that is the LoRa↔TCP/UDP bridge.
