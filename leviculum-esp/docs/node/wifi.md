# `node::wifi` — the WiFi carrier

`src/node/wifi/{mod,tcp,udp}.rs` (feature `wifi`)

Reticulum over the ESP32-S3's station radio. LoRa stays interface 0;
WiFi adds `wifi_udp` (id 1, MTU 1400) and `wifi_tcp` (id 2, MTU 1500).
With transport enabled the node bridges LoRa ↔ TCP/UDP for the mesh.

## Shape

esp-radio brings up the station; embassy-net (smoltcp) is the stack.
One Embassy task per moving part:

- `connection` — keeps the station on the settings' network: applies
  the config, joins, re-joins on disconnect (5 s retry cadence), and
  re-applies whenever the settings change.
- `net_task` — the embassy-net runner.
- `udp::task` — waits for DHCP, binds the settings' port, then
  `select`s between datagrams and the `TX` channel forever; rebinds on
  a changed port, retries a failed bind every second.
- `tcp::task` — dials the settings' target, pumps the HDLC stream,
  reconnects on loss; with no target it idles. Always spawned — the
  target is runtime, not compile-time.

Framing is HDLC (`FLAG` + escaped payload) on both sockets, matching
the reference `UDPInterface`/`TCPClientInterface`. Deframed packets are
published to `INBOUND`, a bounded channel (8) of
`(InterfaceId, Vec<u8>)` the main loop drains into
`node.handle_packet`. Task-side events travel on `LOG`, a channel of
lines the loop prints as `[WIFI] …` (a formed string is leaked —
bounded to a few bring-up sites; there is no log object in the tasks).

## Egress routing

`Ctx::dispatch` calls `wifi::egress(out)` first:

- `SendPacket` to `UDP_IFACE`/`TCP_IFACE` → HDLC-frame → the task's
  `TX` channel.
- `Broadcast` → every medium `exclude_iface` does not name (a frame to
  each socket plus the action left for LoRa).
- Everything else is returned for the LoRa carrier.

## Configuration: runtime, not compile-time

`settings` (`src/node/wifi/settings.rs`) holds the running values,
seeded at boot from the flash slot (`WFI2` record in `store.rs`,
sector `0x703000`) or — on a fresh flash — from the compiled defaults:

| env (at build, the defaults) | meaning | default |
|---|---|---|
| `LEVICULUM_WIFI_SSID` | network to join | unset → WiFi dark |
| `LEVICULUM_WIFI_PSK` | WPA2 password | empty → open network |
| `LEVICULUM_UDP_PORT` | UDP listen/forward port | 4968 |
| `LEVICULUM_UDP_FORWARD` | datagram destination | `255.255.255.255` (LAN broadcast, like the reference UDPInterface) |
| `LEVICULUM_TCP_TARGET` | `a.b.c.d:port` to dial | unset → no TCP task |

### Debug-port commands

Lines starting with `WIFI` are the carrier's own console (`mainloop`
routes them to `wifi::on_line` before the app sees them):

```
WIFI STATUS                      print the running settings
WIFI SSID <name|off>             join a network (off = radio dark)
WIFI PSK <pass|none>             WPA2 passphrase (none = open)
WIFI UDP <port> [<a.b.c.d>|off]  UDP port and forward address
WIFI TCP <a.b.c.d:port|off>      TCP client target
WIFI SAVE                        persist the running settings to flash
WIFI RESET                       back to the compiled defaults
```

### How a change reaches the air

Every change bumps a version counter (`cfg_version`); the tasks
re-read the settings each cycle and react:

- `connection` — disconnects, re-applies `controller.set_config` with
  the new SSID/PSK, re-connects; with no SSID the radio stays dark.
- `tcp::task` — with no target it idles; a changed target drops the
  link and re-dials the new endpoint.
- `udp::task` — rebinds on a changed port.

Persistence: `WIFI SAVE` writes a CRC-guarded record to flash; the
next boot starts from it. The command grammar and the frame codec are
pure and host-tested (`rustc --test src/node/wifi/settings.rs`).

`start` also swaps the allocator story: with `wifi`, `esp-alloc` is the
single global heap (192 KiB) shared by the driver and Rust `alloc`
instead of the separate 128 KiB `embedded-alloc` heap.

## The TCP task's one subtlety

`TcpSocket::write` may accept fewer bytes than offered; a short write
would silently drop the tail and corrupt the HDLC stream for the rest
of the connection. `send` loops until the whole frame is out, then
flushes.
