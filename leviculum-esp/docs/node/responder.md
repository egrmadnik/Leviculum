# `node::responder` — the shared LXMF inbox

`src/node/responder.rs` (compiled with `lxmf-echo` or `lxmf-blink`)

An `lxmf.delivery` inbox that answers every message with a reply an app
computes from the message text. The app supplies two things — an LXMF
title and `fn(&[u8]) -> Vec<u8>` — and the responder does the LXMF
work:

- `register` creates the full delivery destination under the node's
  identity (`LxmfNode::delivery_destination`) and registers packet and
  resource delivery, direct links, and proofs. Message size cap 8 KiB;
  no bzip2 on the board.
- `on_events` feeds each batch of `NodeEvent`s to `LxmfNode::handle_event`,
  dispatches the resulting core actions, and maps LXMF events:
  - `MessageReceived` → compute the reply, `send_or_queue`
  - `PeerAnnounced` / `DirectLinkEstablished` → `flush` the pending queue
  - `InboundRejected` → log

## Send policy

A reply goes over a direct link when one is open to the sender,
otherwise opportunistically. On failure:

- `DirectLinkUnavailable` on a direct send → rebuild as opportunistic
  and retry once.
- `Send` / `UnknownPeer` / `DirectLinkUnavailable` → queue the reply
  (`Pending { dest, content, timestamp }`), then ask for a path:
  `ensure_direct_link`, or a bare `request_path` when the peer's key is
  not yet known — a relay answers with the announce that carries it.
- Anything else → permanent, logged, dropped.

The reply function runs in the main loop once per message — it must not
block and must not wait for hardware.
