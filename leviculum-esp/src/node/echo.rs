//! Echo app: replies to every LXMF message with its own text prefixed by
//! `echo: `. Selected by the `lxmf-echo` cargo feature.

use alloc::vec::Vec;

use leviculum_core::node::NodeEvent;
use leviculum_core::DestinationHash;

use crate::node::app::{App, Ctx};
use crate::node::board::AppWiring;
use crate::node::responder::Responder;

/// LXMF title of an echo reply.
pub const TITLE: &[u8] = b"echo";

pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + content.len());
    out.extend_from_slice(b"echo: ");
    out.extend_from_slice(content);
    out
}

pub struct EchoApp {
    responder: Responder,
}

impl App for EchoApp {
    fn start(ctx: &mut Ctx<'_>, _w: AppWiring) -> Self {
        EchoApp {
            responder: Responder::register(ctx, TITLE, reply)
                .expect("[LXMF] inbox registration failed"),
        }
    }

    fn delivery_hash(&self) -> DestinationHash {
        self.responder.delivery_hash()
    }

    fn announce_data(&self) -> &'static [u8] {
        b"leviculum-xiao_s3-echo"
    }

    fn on_events(&mut self, ctx: &mut Ctx<'_>, events: &[NodeEvent]) {
        self.responder.on_events(ctx, events);
    }
}
