//! Echo app: replies to every LXMF message with its own text prefixed by
//! `echo: `. Selected by the `lxmf-echo` cargo feature.

use alloc::vec::Vec;

/// LXMF title of an echo reply.
pub const TITLE: &[u8] = b"echo";

pub fn reply(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + content.len());
    out.extend_from_slice(b"echo: ");
    out.extend_from_slice(content);
    out
}
