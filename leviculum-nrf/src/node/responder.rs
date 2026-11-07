//! LXMF responder: an `lxmf.delivery` inbox that answers every message it
//! receives with a reply an app computes from the message text.
//!
//! Used by the apps behind the `lxmf-echo` and `lxmf-blink` cargo
//! features. The inbox replaces the announce-only delivery destination the
//! telemetry path registers (same identity, so the same destination hash);
//! everything else of the LNode keeps running around it.
//!
//! The reply goes out over a direct link when one is open to the sender,
//! otherwise opportunistically. With no known path the reply waits in a
//! queue and a path/link is requested; the queue drains on the peer's
//! announce or when a link comes up.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use leviculum_core::ifac::IfacConfig;
use leviculum_core::node::NodeEvent;
use leviculum_core::traits::Interface;
use leviculum_core::transport::dispatch_actions;
use leviculum_core::DestinationHash;
use leviculum_lxmf::{
    DeliveryMethod, LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent, Message,
};

use crate::ble::BleInterface;
use crate::interface::EmbeddedInterface;
use crate::log_critical;
use crate::lora::LoRaInterface;
use crate::node::reticulum::Node;

/// The three interfaces and the IFAC table, borrowed for one dispatch.
pub struct Ifaces<'a, 'i> {
    pub serial: &'a mut EmbeddedInterface<'i>,
    pub lora: &'a mut LoRaInterface,
    pub ble: &'a mut BleInterface,
    pub ifac: &'a BTreeMap<usize, IfacConfig>,
}

impl Ifaces<'_, '_> {
    fn dispatch(&mut self, site: &str, node: &mut Node, actions: Vec<leviculum_core::transport::Action>) {
        let mut ifaces: [&mut dyn Interface; 3] = [self.serial, self.lora, self.ble];
        let dispatched = dispatch_actions(&mut ifaces, actions, self.ifac);
        crate::dispatch::settle(site, node, &dispatched);
    }
}

/// A reply waiting for a path to its recipient.
struct PendingEcho {
    dest: [u8; 16],
    content: Vec<u8>,
    timestamp: f64,
}

enum EchoResult {
    Sent,
    /// No path yet: keep the reply and ask for one.
    Queued,
    /// Permanent error, not worth waiting for.
    Failed(&'static str),
}

/// The responder inbox: the LXMF node plus replies still waiting for a path.
pub struct Responder {
    lxmf: LxmfNode,
    delivery_hash: DestinationHash,
    pending: Vec<PendingEcho>,
    /// LXMF title of every reply.
    title: &'static [u8],
    /// Turns a received message's content into the reply's content. Runs
    /// once per message, in the main loop: it must not block.
    reply: fn(&[u8]) -> Vec<u8>,
}

impl Responder {
    /// Register the full `lxmf.delivery` inbox (packet and resource
    /// delivery, direct links, proof for every message).
    pub fn register(
        node: &mut Node,
        title: &'static [u8],
        reply: fn(&[u8]) -> Vec<u8>,
    ) -> Option<Self> {
        let dest = LxmfNode::delivery_destination(node.identity().clone()).ok()?;
        let delivery_hash = *dest.hash();
        // No bzip2 on the board: smaller flash.
        let config = LxmfNodeConfig {
            max_incoming_resource_size: Some(crate::MAX_INCOMING_RESOURCE_BYTES as u64),
            max_outgoing_delivery_size: Some(crate::MAX_INCOMING_RESOURCE_BYTES as u64),
            auto_compress_resources: false,
        };
        let lxmf = LxmfNode::register(node, dest, config).ok()?;
        let d = delivery_hash.as_bytes();
        log_critical!(
            "[LXMF] inbox ready delivery_hash={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
            d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
        );
        Some(Self { lxmf, delivery_hash, pending: Vec::new(), title, reply })
    }

    pub fn delivery_hash(&self) -> DestinationHash {
        self.delivery_hash
    }

    /// Feed core events to the inbox; answer what arrives.
    pub fn on_events(&mut self, node: &mut Node, events: &[NodeEvent], io: &mut Ifaces<'_, '_>) {
        for ev in events {
            let out = match self.lxmf.handle_event(node, ev) {
                Ok(out) => out,
                Err(_) => continue,
            };
            if !out.core.actions.is_empty() {
                io.dispatch("lxmf", node, out.core.actions);
            }
            for lxmf_ev in out.events {
                match lxmf_ev {
                    LxmfNodeEvent::MessageReceived(msg) => {
                        let s = msg.source_hash;
                        log_critical!(
                            "[LXMF] msg rx src={:02x}{:02x}{:02x}{:02x} len={} verify={:?}",
                            s[0], s[1], s[2], s[3], msg.content.len(), msg.verification
                        );
                        self.send_or_queue(node, io, s, &msg.content, msg.timestamp);
                    }
                    LxmfNodeEvent::InboundRejected { method, reason } => {
                        log_critical!("[LXMF] inbound rejected method={:?} reason={}", method, reason);
                    }
                    LxmfNodeEvent::PeerAnnounced { destination } => {
                        let b = destination.as_bytes();
                        log_critical!("[LXMF] peer announced {:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3]);
                        self.flush(node, io);
                    }
                    LxmfNodeEvent::DirectLinkEstablished { is_initiator, .. } => {
                        log_critical!("[LXMF] link established initiator={}", is_initiator);
                        self.flush(node, io);
                    }
                    _ => {}
                }
            }
        }
    }

    fn send_or_queue(
        &mut self,
        node: &mut Node,
        io: &mut Ifaces<'_, '_>,
        dest: [u8; 16],
        original: &[u8],
        timestamp: f64,
    ) {
        let content = (self.reply)(original);
        match self.try_send(node, io, dest, &content, timestamp) {
            EchoResult::Sent => {
                log_critical!("[LXMF] echo sent dest={:02x}{:02x}{:02x}{:02x}", dest[0], dest[1], dest[2], dest[3]);
            }
            EchoResult::Queued => {
                log_critical!("[LXMF] echo queued (no path) dest={:02x}{:02x}{:02x}{:02x}", dest[0], dest[1], dest[2], dest[3]);
                self.pending.push(PendingEcho { dest, content, timestamp });
            }
            EchoResult::Failed(reason) => {
                log_critical!("[LXMF] echo FAILED dest={:02x}{:02x}{:02x}{:02x} reason={}", dest[0], dest[1], dest[2], dest[3], reason);
            }
        }
    }

    fn flush(&mut self, node: &mut Node, io: &mut Ifaces<'_, '_>) {
        let queued = core::mem::take(&mut self.pending);
        for p in queued {
            match self.try_send(node, io, p.dest, &p.content, p.timestamp) {
                EchoResult::Sent => {
                    log_critical!("[LXMF] echo (queued) sent dest={:02x}{:02x}{:02x}{:02x}", p.dest[0], p.dest[1], p.dest[2], p.dest[3]);
                }
                EchoResult::Queued => self.pending.push(p),
                EchoResult::Failed(_) => {}
            }
        }
    }

    fn build(
        &self,
        node: &Node,
        dest: [u8; 16],
        content: &[u8],
        timestamp: f64,
        method: DeliveryMethod,
    ) -> Result<Message, ()> {
        Message::create(
            dest,
            *self.delivery_hash.as_bytes(),
            node.identity(),
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
        node: &mut Node,
        io: &mut Ifaces<'_, '_>,
        dest: [u8; 16],
        content: &[u8],
        timestamp: f64,
    ) -> EchoResult {
        let dest_dh = DestinationHash::new(dest);
        let method = if self.lxmf.direct_link(&dest_dh).is_some() {
            DeliveryMethod::Direct
        } else {
            DeliveryMethod::Opportunistic
        };
        let Ok(reply) = self.build(node, dest, content, timestamp, method) else {
            return EchoResult::Failed("message build failed");
        };
        match self.lxmf.send(node, &reply) {
            Ok(out) => {
                if !out.core.actions.is_empty() {
                    io.dispatch("lxmf-echo-tx", node, out.core.actions);
                }
                EchoResult::Sent
            }
            // The link closed under us: retry as a single packet.
            Err(LxmfNodeError::DirectLinkUnavailable) if method == DeliveryMethod::Direct => {
                log_critical!("[LXMF] direct link gone, retry opportunistic");
                let Ok(reply) =
                    self.build(node, dest, content, timestamp, DeliveryMethod::Opportunistic)
                else {
                    return EchoResult::Failed("message build (retry) failed");
                };
                match self.lxmf.send(node, &reply) {
                    Ok(out) => {
                        if !out.core.actions.is_empty() {
                            io.dispatch("lxmf-echo-tx2", node, out.core.actions);
                        }
                        EchoResult::Sent
                    }
                    Err(e) => self.queue_or_fail(node, io, dest_dh, e),
                }
            }
            Err(e) => self.queue_or_fail(node, io, dest_dh, e),
        }
    }

    /// A send failed: ask for a path or link where waiting makes sense.
    fn queue_or_fail(
        &mut self,
        node: &mut Node,
        io: &mut Ifaces<'_, '_>,
        dest_dh: DestinationHash,
        err: LxmfNodeError,
    ) -> EchoResult {
        log_critical!("[LXMF] send err={:?} queuing+path-req", err);
        match err {
            LxmfNodeError::Send(_)
            | LxmfNodeError::UnknownPeer
            | LxmfNodeError::DirectLinkUnavailable => {
                match self.lxmf.ensure_direct_link(node, dest_dh) {
                    Ok((_, path_out)) if !path_out.core.actions.is_empty() => {
                        io.dispatch("lxmf-path-req", node, path_out.core.actions);
                    }
                    // No key for the peer: a bare path request makes a relay
                    // answer with the announce that carries it.
                    _ => {
                        let pr = node.request_path(&dest_dh);
                        if !pr.actions.is_empty() {
                            io.dispatch("lxmf-path-req2", node, pr.actions);
                        }
                    }
                }
                EchoResult::Queued
            }
            _ => EchoResult::Failed("permanent send error"),
        }
    }
}
