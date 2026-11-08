# WiFi interfaces on the ESP32-S3

With the `wifi` cargo feature an ESP LNode runs two Reticulum interfaces
on the chip's own WiFi radio alongside LoRa (`InterfaceId(0)`):

| id | name | what it is |
| --- | --- | --- |
| 1 | `wifi_udp` | one HDLC-framed packet per datagram; bound to a port, forwarded to a configured IPv4 — the broadcast address by default, so nodes on the same LAN hear each other (the reference `UDPInterface`'s shape) |
| 2 | `wifi_tcp` | an HDLC-framed stream to one configured `host:port`, reconnecting on loss (the client half of `TCPClientInterface`) |

Build it:

```sh
cd leviculum-esp && . ~/export-esp.sh
LEVICULUM_WIFI_SSID=mynet LEVICULUM_WIFI_PSK=secret \
LEVICULUM_TCP_TARGET=192.168.1.10:4242 \
cargo build --release --bin xiao_s3 --features wifi
```

Everything below the task boundary is **compile-time** — the network is a
lab fixture, not a runtime setting:

| env (at build) | meaning | default |
| --- | --- | --- |
| `LEVICULUM_WIFI_SSID` | network to join | unset → WiFi dark, no interfaces registered |
| `LEVICULUM_WIFI_PSK` | WPA2 password | empty → open network |
| `LEVICULUM_UDP_PORT` | UDP listen and forward port | `4968` |
| `LEVICULUM_UDP_FORWARD` | IPv4 datagrams go to | `255.255.255.255` |
| `LEVICULUM_TCP_TARGET` | `a.b.c.d:port` to dial | unset → no TCP task |

## How it is put together

```text
   STA connect task (reconnects)      embassy-net runner task
            │                                  │
            ▼                                  ▼
      esp-radio WifiController        smoltcp stack (DHCP)
                                                 │
              ┌──────────── UDP task ────────────┤─────────── TCP task ───────────┐
              │  bind :4968 · recv → Deframer    │   connect target · stream →    │
              │  TX channel → frame → forward    │   Deframer · TX channel → write │
              └──────┬───────────────────────────┴────────────┬───────────────────┘
                     │ INBOUND channel: (iface, packet)       │ TX channels
                     ▼                                        ▲
              mainloop drains → node.handle_packet(iface)    │
              Ctx::dispatch → wifi::egress routes SendPacket/Broadcast ──┘
```

- `node/wifi/mod.rs` — station bring-up, the stack, the channels, egress
  routing, `InterfaceId`s 1 and 2.
- `node/wifi/udp.rs`, `node/wifi/tcp.rs` — the socket tasks. Events they
  cannot print themselves (the log lives in the loop) travel a `LOG`
  channel and surface as `[WIFI]` lines: `sta=connected`, `net=up`,
  `udp=listening`, `tcp=connected`, …
- Outbound `SendPacket{iface}` goes only to that interface; `Broadcast`
  goes to every medium its `exclude_iface` does not name. `carriers.rs`
  now drops non-LoRa actions rather than airing them — a latent bug the
  second interface exposed.

## Memory and crates

`wifi` pulls `esp-radio 1.0.0-beta.1` (pairs `esp-hal ~1.2`, `esp-rtos
0.4`; beta.2 is yanked), `embassy-net 0.9`, and `esp-alloc`. Under the
feature esp-alloc *is* the global allocator — one 192 KiB heap covers the
driver's internal mallocs and Rust's `alloc`, and the embedded-alloc
heap is compiled out.

## Limits

- IPv4 only; no TLS — the mesh's own signatures are the security layer.
- UDP forward is a single address (broadcast or unicast), not a list.
- TCP is a client; there is no listener.
- Credentials are compiled in, not stored in flash — reflashing with new
  env is how a network change reaches a board.
