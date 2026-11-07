//! LXMF responder: an `lxmf.delivery` inbox that answers every message it
//! receives with a reply an app computes from the message text.
//!
//! Used by the apps behind the `lxmf-echo` and `lxmf-blink` cargo
//! features; the same module, with the same shape, as
//! `leviculum-nrf/src/node/responder.rs`. The app supplies a LXMF title and
//! a function from received text to reply text; the responder does the LXMF
//! work (registering the destination under the node's identity, proofs,
//! direct links, queueing a reply when there is no path yet and retrying
//! when the peer announces).
//!
//! The reply goes out over a direct link when one is open to the sender,
//! otherwise opportunistically. With no known path the reply waits in a
//! queue and a path request goes out; the queue drains when the peer
//! announces or a link comes up.
//!
//! The reply function runs in the main loop, once per message. It must not
//! block and must not wait for hardware.

use alloc::vec::Vec;

use leviculum_core::node::NodeEvent;
use leviculum_core::DestinationHash;
use leviculum_lxmf::{
    DeliveryMethod, LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent, Message,
};

use crate::node::app::Ctx;

/// The largest message either way, bytes. The ESP heap is small.
const MAX_MESSAGE_BYTES: u64 = 8 * 1024;

/// A reply waiting for a path to its recipient.
struct Pending {
    dest: [u8; 16],
    content: Vec<u8>,
    timestamp: f64,
}

enum Sent {
    Yes,
    /// No path yet: keep the reply and ask for one.
    Queued,
    /// Permanent error, not worth waiting for.
    Failed(&'static str),
}

/// The responder inbox: the LXMF node plus replies still waiting for a path.
pub struct Responder {
    lxmf: LxmfNode,
    delivery_hash: DestinationHash,
    pending: Vec<Pending>,
    /// LXMF title of every reply.
    title: &'static [u8],
    /// Turns a received message's content into the reply's content.
    reply: fn(&[u8]) -> Vec<u8>,
}

impl Responder {
    /// Register the full `lxmf.delivery` inbox (packet and resource
    /// delivery, direct links, proof for every message).
    pub fn register(
        ctx: &mut Ctx<'_>,
        title: &'static [u8],
        reply: fn(&[u8]) -> Vec<u8>,
    ) -> Option<Self> {
        let dest = LxmfNode::delivery_destination(ctx.identity.clone()).ok()?;
        let delivery_hash = *dest.hash();
        // No bzip2 on the board: smaller flash.
        let config = LxmfNodeConfig {
            max_incoming_resource_size: Some(MAX_MESSAGE_BYTES),
            max_outgoing_delivery_size: Some(MAX_MESSAGE_BYTES),
            auto_compress_resources: false,
        };
        let lxmf = LxmfNode::register(ctx.node, dest, config).ok()?;
        let d = delivery_hash.as_bytes();
        ctx.log.line(
            "[LXMF] ",
            format_args!(
                "inbox ready delivery_hash={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
                d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
            ),
        );
        Some(Self {
            lxmf,
            delivery_hash,
            pending: Vec::new(),
            title,
            reply,
        })
    }

    pub fn delivery_hash(&self) -> DestinationHash {
        self.delivery_hash
    }

    /// Feed core events to the inbox; answer what arrives.
    pub fn on_events(&mut self, ctx: &mut Ctx<'_>, events: &[NodeEvent]) {
        for ev in events {
            let out = match self.lxmf.handle_event(ctx.node, ev) {
                Ok(out) => out,
                Err(_) => continue,
            };
            if !out.core.actions.is_empty() {
                ctx.dispatch(out.core);
            }
            for lxmf_ev in out.events {
                match lxmf_ev {
                    LxmfNodeEvent::MessageReceived(msg) => {
                        let s = msg.source_hash;
                        ctx.log.line(
                            "[LXMF] ",
                            format_args!(
                                "msg rx src={:02x}{:02x}{:02x}{:02x} len={} verify={:?}",
                                s[0],
                                s[1],
                                s[2],
                                s[3],
                                msg.content.len(),
                                msg.verification
                            ),
                        );
                        self.send_or_queue(ctx, s, &msg.content, msg.timestamp);
                    }
                    LxmfNodeEvent::InboundRejected { method, reason } => {
                        ctx.log.line(
                            "[LXMF] ",
                            format_args!("inbound rejected method={:?} reason={}", method, reason),
                        );
                    }
                    LxmfNodeEvent::PeerAnnounced { destination } => {
                        let b = destination.as_bytes();
                        ctx.log.line(
                            "[LXMF] ",
                            format_args!(
                                "peer announced {:02x}{:02x}{:02x}{:02x}",
                                b[0], b[1], b[2], b[3]
                            ),
                        );
                        self.flush(ctx);
                    }
                    LxmfNodeEvent::DirectLinkEstablished { is_initiator, .. } => {
                        ctx.log.line(
                            "[LXMF] ",
                            format_args!("link established initiator={}", is_initiator),
                        );
                        self.flush(ctx);
                    }
                    _ => {}
                }
            }
        }
    }

    fn send_or_queue(
        &mut self,
        ctx: &mut Ctx<'_>,
        dest: [u8; 16],
        original: &[u8],
        timestamp: f64,
    ) {
        let content = (self.reply)(original);
        match self.try_send(ctx, dest, &content, timestamp) {
            Sent::Yes => ctx.log.line(
                "[LXMF] ",
                format_args!(
                    "reply sent dest={:02x}{:02x}{:02x}{:02x}",
                    dest[0], dest[1], dest[2], dest[3]
                ),
            ),
            Sent::Queued => {
                ctx.log.line(
                    "[LXMF] ",
                    format_args!(
                        "reply queued (no path) dest={:02x}{:02x}{:02x}{:02x}",
                        dest[0], dest[1], dest[2], dest[3]
                    ),
                );
                self.pending.push(Pending {
                    dest,
                    content,
                    timestamp,
                });
            }
            Sent::Failed(reason) => ctx.log.line(
                "[LXMF] ",
                format_args!(
                    "reply FAILED dest={:02x}{:02x}{:02x}{:02x} reason={}",
                    dest[0], dest[1], dest[2], dest[3], reason
                ),
            ),
        }
    }

    fn flush(&mut self, ctx: &mut Ctx<'_>) {
        let queued = core::mem::take(&mut self.pending);
        for p in queued {
            match self.try_send(ctx, p.dest, &p.content, p.timestamp) {
                Sent::Yes => ctx.log.line(
                    "[LXMF] ",
                    format_args!(
                        "reply (queued) sent dest={:02x}{:02x}{:02x}{:02x}",
                        p.dest[0], p.dest[1], p.dest[2], p.dest[3]
                    ),
                ),
                Sent::Queued => self.pending.push(p),
                Sent::Failed(_) => {}
            }
        }
    }

    fn build(
        &self,
        ctx: &Ctx<'_>,
        dest: [u8; 16],
        content: &[u8],
        timestamp: f64,
        method: DeliveryMethod,
    ) -> Result<Message, ()> {
        Message::create(
            dest,
            *self.delivery_hash.as_bytes(),
            ctx.node.identity(),
            timestamp,
            self.title.to_vec(),
            content.to_vec(),
            Vec::new(),
            method,
        )
        .map_err(|_| ())
    }

    fn try_send(
        &mut self,
        ctx: &mut Ctx<'_>,
        dest: [u8; 16],
        content: &[u8],
        timestamp: f64,
    ) -> Sent {
        let dest_dh = DestinationHash::new(dest);
        let method = if self.lxmf.direct_link(&dest_dh).is_some() {
            DeliveryMethod::Direct
        } else {
            DeliveryMethod::Opportunistic
        };
        let Ok(reply) = self.build(ctx, dest, content, timestamp, method) else {
            return Sent::Failed("message build failed");
        };
        match self.lxmf.send(ctx.node, &reply) {
            Ok(out) => {
                if !out.core.actions.is_empty() {
                    ctx.dispatch(out.core);
                }
                Sent::Yes
            }
            // The link closed under us: retry as a single packet.
            Err(LxmfNodeError::DirectLinkUnavailable) if method == DeliveryMethod::Direct => {
                let Ok(reply) =
                    self.build(ctx, dest, content, timestamp, DeliveryMethod::Opportunistic)
                else {
                    return Sent::Failed("message build (retry) failed");
                };
                match self.lxmf.send(ctx.node, &reply) {
                    Ok(out) => {
                        if !out.core.actions.is_empty() {
                            ctx.dispatch(out.core);
                        }
                        Sent::Yes
                    }
                    Err(e) => self.queue_or_fail(ctx, dest_dh, e),
                }
            }
            Err(e) => self.queue_or_fail(ctx, dest_dh, e),
        }
    }

    /// A send failed: ask for a path or link where waiting makes sense.
    fn queue_or_fail(
        &mut self,
        ctx: &mut Ctx<'_>,
        dest_dh: DestinationHash,
        err: LxmfNodeError,
    ) -> Sent {
        ctx.log.line(
            "[LXMF] ",
            format_args!("send err={:?} queuing+path-req", err),
        );
        match err {
            LxmfNodeError::Send(_)
            | LxmfNodeError::UnknownPeer
            | LxmfNodeError::DirectLinkUnavailable => {
                match self.lxmf.ensure_direct_link(ctx.node, dest_dh) {
                    Ok((_, path_out)) if !path_out.core.actions.is_empty() => {
                        ctx.dispatch(path_out.core);
                    }
                    // No key for the peer: a bare path request makes a
                    // relay answer with the announce that carries it.
                    _ => {
                        let pr = ctx.node.request_path(&dest_dh);
                        if !pr.actions.is_empty() {
                            ctx.dispatch(pr);
                        }
                    }
                }
                Sent::Queued
            }
            _ => Sent::Failed("permanent send error"),
        }
    }
}
