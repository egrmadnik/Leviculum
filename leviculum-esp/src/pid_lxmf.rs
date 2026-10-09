//! The PID loop's LXMF layer — real LXMF messages, no transport yet.
//!
//! What a remote node sends *to* this board is an LXMF [`Message`]
//! addressed to our `lxmf.delivery` destination whose `content` is a
//! `PIDC` frame; what this board reports back is an LXMF message to the
//! remote's own delivery hash whose `content` is a `PIDS` frame. Both
//! are real LXMF: full destination hashing, Ed25519 signatures, the
//! msgpack payload — produced by [`leviculum_lxmf::Message`], not a
//! look-alike.
//!
//! What does **not** exist yet is what moves them: the `xiao_s3` target
//! has no Reticulum transport — no LoRa packet path, no delivery
//! engine. The functions below are exactly the calls a transport makes:
//! the same `decode_config_message` the radio path will feed, the same
//! `build_status_message` whose bytes it will radiate. Until then the
//! debug port carries the frames — `PID LXMF <hex>` in, hex logs out —
//! which is how the codec itself gets proven on hardware.

extern crate alloc;
use alloc::vec::Vec;

use leviculum_core::Identity;
use leviculum_lxmf::{message::DeliveryMethod, LxmfNode, Message};

use crate::pid::{Config, Status};

/// The destination hash this node answers PID configuration on — its
/// own `lxmf.delivery`, the same derivation the nRF boards register
/// (name hash + identity hash, truncated to 16 B).
///
/// `delivery_destination_without_inbox` builds the full `Destination`;
/// only the hash is needed here, so it is constructed and discarded.
/// The identity moves in because the API takes ownership — cloning is
/// the caller's cheap fix.
pub fn delivery_hash(identity: Identity) -> Option<[u8; 16]> {
    let dest = LxmfNode::delivery_destination_without_inbox(identity).ok()?;
    Some(*dest.hash().as_bytes())
}

/// What an inbound PID command turns out to be.
pub enum Inbound {
    /// A well-formed config: apply it, and remember the sender as the
    /// report target — the remote that asked is the remote that hears.
    Config(Config, [u8; 16]),
    /// The bytes parsed as an LXMF message but were not for us, or the
    /// content was not a `PIDC` frame.
    NotOurs,
    /// Not an LXMF message at all.
    BadFrame,
}

/// Decode an inbound LXMF delivery into a PID config.
///
/// `data` is the **opportunistic wire shape** — `NodeEvent::
/// PacketReceived` hands up the decrypted payload, which for an
/// opportunistic LXMF send is `packed[16..]` (destination prefix
/// stripped), the same bytes `screen_telemetry_request` consumes on
/// the nRF boards. `inferred_destination = our_dest` restores the
/// stripped hash so the unpacker's destination check is real.
///
/// The signature is **not verified** (`source=None` → the message
/// comes back `Verification::Unverified`): we never had the sender's
/// public identity to check against. That is a real hole, stated
/// rather than hidden — anyone who can reach this destination can
/// retune the loop until the mgmt allow-list lands. The `PIDC` frame's
/// own CRC still applies after unpack, so a corrupted payload is still
/// refused; what is missing is auth, not integrity.
pub fn decode_config_message(data: &[u8], our_dest: &[u8; 16]) -> Inbound {
    let msg = match Message::unpack(data, Some(*our_dest), None, DeliveryMethod::Opportunistic) {
        Ok(m) => m,
        Err(_) => return Inbound::BadFrame,
    };
    if &msg.destination_hash != our_dest {
        return Inbound::NotOurs;
    }
    match Config::decode(&msg.content) {
        Ok(cfg) if cfg.sample_ms > 0 && cfg.window_ms > 0 => Inbound::Config(cfg, msg.source_hash),
        _ => Inbound::NotOurs,
    }
}

/// Build the status report LXMF message: our delivery hash as source,
/// the remote's as destination, a `PIDS` frame as content. Returns the
/// **opportunistic on-air bytes** (`packed[16..]`) — the payload a
/// `send_single_packet` call encrypts toward the destination, matching
/// how the nRF telemetry path emits Telemeter reports.
///
/// `timestamp_s` is the message timestamp — a board with no clock
/// anchor stamps uptime seconds, which is honest (it *is* the node's
/// time) and clearly not a wall clock; consumers must not sort reports
/// by it across a reboot.
///
/// `None` when signing fails — the identity is the only thing that can,
/// and it is checked once at boot.
pub fn build_status_message(
    identity: &Identity,
    our_dest: &[u8; 16],
    target_dest: &[u8; 16],
    status: &Status,
    timestamp_s: f64,
) -> Option<Vec<u8>> {
    let msg = Message::create(
        *target_dest,
        *our_dest,
        identity,
        timestamp_s,
        Vec::new(),
        status.encode().to_vec(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .ok()?;
    msg.on_air().ok()
}
