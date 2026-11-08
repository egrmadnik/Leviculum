# `node::carriers` — the LoRa carrier

`src/node/carriers.rs`

Interface 0: bring the SX1262 up, move RNode-framed packets in and out.

## `start`

Bring-up runs in the order the `leviculum_core::sx126x` sequences expect
(identical to the nRF boards):

1. NSS high **before** the SPI bus exists — a low CS while the SoC
   boots is the start of a command the chip will wait to finish.
2. BUSY and DIO1 as floating inputs (the radio drives them); RXEN parked
   low until `listen` asserts it.
3. SPI mode 0 at the board's `spi_freq_hz`, into async mode.
4. `radio.init(freq)` → `radio.configure(...)` → `radio.listen()`
   (continuous RX).

An SPI config failure is fatal (`boot::halt_blinking`). A radio that
fails init or configure leaves `Carrier { radio: None }` — the board
logs `[LORA] state=down` and runs everything else. A sensor node
without a mesh is degraded, not dead.

## `ingest` — LoRa → node

One poll per loop pass. A completed frame is fed to the
`SplitReassembler` (a Reticulum packet may span several 254-byte LoRa
frames); a complete packet goes to `node.handle_packet(InterfaceId(0), …)`
and the returned `TickOutput` goes back to the caller. Bad CRCs and bus
errors are logged; a bus error also re-arms `listen`.

## `dispatch` — node → air

Each `TickOutput` action becomes airtime:

- `Action::SendPacket` only when `iface == InterfaceId(0)` — packets
  addressed to the WiFi interfaces are not ours.
- `Action::Broadcast` unless `exclude_iface == InterfaceId(0)`.

Each packet is split by `rnode::build_lora_frames` with the carrier's
sequence nibble (`tx_seq << 4`), transmitted with a timeout computed
from the channel parameters (`sx126x::tx_timeout_ms`), and `listen` is
re-armed after every transmit — the chip lands in standby after TX.

## The two paths

```
 RX path (per loop pass, radio up):
   radio.poll_rx ──► Rx::Frame ──► SplitReassembler.feed ──► complete packet?
        │                (≤ 254 B)     (reassembles across frames)     │
        │  Rx::BadCrc ──► log rx err=crc ──────────────────────────────┤ no
        │  Rx::Idle ────► nothing                                       │
        ▼                                                              │ yes
   node.handle_packet(InterfaceId(0), packet) ◄────────────────────────┘
        │
        ▼
   TickOutput ──► handle(): app.on_events first, then dispatch

 TX path (per loop pass):
   TickOutput.actions ──► SendPacket/Broadcast ──► iface 0? ──no──► skip
        │                                          │yes
        │                                          ▼
        │                            build_lora_frames (254 B splits,
        │                             tx_seq nibble in the header)
        │                                          │
        │                                          ▼
        │                            radio.transmit (timeout from
        │                             bw/sf/cr/preamble) ──► listen()
        └─► (WiFi-bound actions were already taken by wifi::egress)
```

## Key points

- **Interface 0 only.** `SendPacket` for another iface is not the
  radio's; a `Broadcast` excludes the interface it came in on.
- **Degraded, not dead.** A radio that fails bring-up leaves
  `Carrier { radio: None }`: the node keeps running, LoRa-less.
- **One reassembler, one transmit at a time** — the loop serialises
  both directions; `listen` is re-armed after every TX.
