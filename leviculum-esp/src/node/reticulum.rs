//! Reticulum: the node's identity and the transport engine around it.
//!
//! One interface, the LoRa radio (`InterfaceId(0)`). The engine is the same
//! `NodeCore` the host daemon and the nRF boards run.

use alloc::boxed::Box;

use esp_hal::rng::Trng;
use leviculum_core::node::{NodeCore, NodeCoreBuilder};
use leviculum_core::{Identity, MemoryStorage};

use crate::{store, UsbLog};

/// `leviculum_core::traits::Clock` over `uptime_ms` — monotonic only;
/// `wall_unix_secs` stays `None`, so announce emission timestamps come
/// from the transport's learned timebase, which is the intended answer on
/// a board with no RTC.
pub struct EspClock;

impl leviculum_core::traits::Clock for EspClock {
    fn now_ms(&self) -> u64 {
        crate::uptime_ms()
    }
}

/// The transport node as every ESP LNode builds it.
pub type Node = NodeCore<Trng, EspClock, MemoryStorage>;

/// Load the identity from its own flash sector, or generate it once with
/// the TRNG and save it — the destination hash a remote node addresses is
/// only stable if the identity is.
///
/// The caller must hold a `TrngSource` for as long as the node lives: the
/// TRNG only unlocks while one exists.
pub fn identity(log: &mut UsbLog<'static>, flash: &mut esp_storage::FlashStorage<'static>) -> Identity {
    match store::load_identity(flash).and_then(|k| Identity::from_private_key_bytes(&k).ok()) {
        Some(id) => {
            log.line("[IDENTITY] ", format_args!("identity=restored"));
            id
        }
        None => {
            let mut trng = Trng::try_new().expect("TRNG source enabled by the caller");
            let id = Identity::generate(&mut trng);
            let persisted = id
                .private_key_bytes()
                .ok()
                .and_then(|k| store::save_identity(flash, &k).ok())
                .is_some();
            log.line(
                "[IDENTITY] ",
                format_args!("identity=generated persisted={persisted}"),
            );
            id
        }
    }
}

/// Build the transport node around `identity`.
pub fn build(identity: &Identity) -> Box<Node> {
    NodeCoreBuilder::new().identity(identity.clone()).build_boxed(
        Trng::try_new().expect("TRNG source enabled by the caller"),
        EspClock,
        MemoryStorage::compact(),
    )
}

/// Name the LoRa interface and set its MTU.
pub fn register_interface(node: &mut Node) {
    node.set_interface_name(0, "lora_sx1262".into());
    // 508 — the RNode HW_MTU the whole fleet agrees on (#390).
    node.set_interface_hw_mtu(0, 508);
}
