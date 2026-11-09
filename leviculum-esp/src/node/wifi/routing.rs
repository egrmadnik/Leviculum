//! The egress routing decision: which medium an outbound action is for.
//!
//! [`egress`](super::egress) turns a `TickOutput` into channel sends and
//! LoRa-carrier actions; this module holds the *decision* that routing
//! makes, expressed over plain integers so the same file a host
//! `rustc --test` compiles can assert the whole matrix (the firmware
//! crate itself cannot run tests — see `docs/architecture.md`).
//!
//! The wire rules, kept in lockstep with `egress`:
//!
//! - a `SendPacket` goes to exactly one interface: 0 (LoRa), 1 (UDP)
//!   or 2 (TCP). A target interface that is none of the three is
//!   dropped by everyone.
//! - a `Broadcast` goes to every medium its `exclude` does not name —
//!   the exclusion is a single interface, and an exclusion of an
//!   interface that is not ours excludes nothing.

/// LoRa stays interface 0 (`leviculum_core::InterfaceId(0)`).
pub const IFACE_LORA: u8 = 0;
/// The UDP interface's id ([`super::UDP_IFACE`]).
pub const IFACE_UDP: u8 = 1;
/// The TCP interface's id ([`super::TCP_IFACE`]).
pub const IFACE_TCP: u8 = 2;

/// The essence of one outbound action, free of transport types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet {
    /// Point-to-point send, addressed to one interface.
    Send { iface: u8 },
    /// Broadcast, excluded from one interface (`None` = exclude none).
    Broadcast { exclude: Option<u8> },
}

/// Which media the packet is routed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// To the UDP socket task.
    pub udp: bool,
    /// To the TCP socket task.
    pub tcp: bool,
    /// Back to the LoRa carrier.
    pub lora: bool,
}

impl Route {
    /// Every medium refused — the packet is dropped.
    #[cfg(test)]
    pub fn none(&self) -> bool {
        !self.udp && !self.tcp && !self.lora
    }
}

/// Decide where one action goes.
pub fn route(packet: Packet) -> Route {
    match packet {
        Packet::Send { iface } => match iface {
            IFACE_LORA => Route { lora: true, ..no() },
            IFACE_UDP => Route { udp: true, ..no() },
            IFACE_TCP => Route { tcp: true, ..no() },
            // A SendPacket for an interface this firmware has not got
            // is not ours to deliver — everyone drops it.
            _ => no(),
        },
        Packet::Broadcast { exclude } => Route {
            udp: exclude != Some(IFACE_UDP),
            tcp: exclude != Some(IFACE_TCP),
            lora: exclude != Some(IFACE_LORA),
        },
    }
}

fn no() -> Route {
    Route {
        udp: false,
        tcp: false,
        lora: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_ids_are_distinct_and_stable() {
        assert_ne!(IFACE_LORA, IFACE_UDP);
        assert_ne!(IFACE_UDP, IFACE_TCP);
        assert_ne!(IFACE_LORA, IFACE_TCP);
        // The wire contract: these are the ids `reticulum` registers.
        assert_eq!(IFACE_LORA, 0);
        assert_eq!(IFACE_UDP, 1);
        assert_eq!(IFACE_TCP, 2);
    }

    #[test]
    fn send_packet_routing_is_exactly_one_medium() {
        assert_eq!(
            route(Packet::Send { iface: IFACE_LORA }),
            Route {
                udp: false,
                tcp: false,
                lora: true
            }
        );
        assert_eq!(
            route(Packet::Send { iface: IFACE_UDP }),
            Route {
                udp: true,
                tcp: false,
                lora: false
            }
        );
        assert_eq!(
            route(Packet::Send { iface: IFACE_TCP }),
            Route {
                udp: false,
                tcp: true,
                lora: false
            }
        );
    }

    #[test]
    fn send_packet_to_an_unknown_iface_is_dropped_by_everyone() {
        for iface in [3, 7, 100, u8::MAX] {
            let r = route(Packet::Send { iface });
            assert!(r.none(), "iface {iface} must be dropped, got {r:?}");
        }
    }

    #[test]
    fn broadcast_without_exclusion_reaches_every_medium() {
        assert_eq!(
            route(Packet::Broadcast { exclude: None }),
            Route {
                udp: true,
                tcp: true,
                lora: true
            }
        );
    }

    #[test]
    fn broadcast_exclusion_removes_exactly_one_medium() {
        assert_eq!(
            route(Packet::Broadcast {
                exclude: Some(IFACE_UDP)
            }),
            Route {
                udp: false,
                tcp: true,
                lora: true
            }
        );
        assert_eq!(
            route(Packet::Broadcast {
                exclude: Some(IFACE_TCP)
            }),
            Route {
                udp: true,
                tcp: false,
                lora: true
            }
        );
        assert_eq!(
            route(Packet::Broadcast {
                exclude: Some(IFACE_LORA)
            }),
            Route {
                udp: true,
                tcp: true,
                lora: false
            }
        );
    }

    #[test]
    fn broadcast_excluding_an_unknown_iface_excludes_nothing() {
        for exclude in [Some(3), Some(9), Some(u8::MAX)] {
            assert_eq!(
                route(Packet::Broadcast { exclude }),
                Route {
                    udp: true,
                    tcp: true,
                    lora: true
                },
                "exclude={exclude:?} must not drop any medium"
            );
        }
    }

    #[test]
    fn a_send_packet_never_leaves_the_carrier_on_the_broadcast_path() {
        // The two packet kinds are disjoint by construction: the
        // broadcast rules cannot leak into the send rules.
        let send = route(Packet::Send { iface: IFACE_UDP });
        let bcast = route(Packet::Broadcast {
            exclude: Some(IFACE_UDP),
        });
        assert!(send.udp && !send.tcp && !send.lora);
        assert!(!bcast.udp && bcast.tcp && bcast.lora);
    }
}
