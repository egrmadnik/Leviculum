//! The LXMF carrier: a `Params` frame arrives as the `content` of a
//! message addressed to this node's `lxmf.delivery`; a `Report` frame
//! leaves as the `content` of a message to the remote. Both are real
//! LXMF — [`leviculum_lxmf::Message`], Ed25519-signed — in the
//! opportunistic representation, so the bytes this module takes and
//! gives are exactly what `NodeCore::send_single_packet` carries and
//! `NodeEvent::PacketReceived` delivers.
//!
//! Authentication: the inbound signature is **not verified** here. A
//! board has no copy of the remote's public identity until the mesh
//! carries one (a known-destinations store or an allow-list — the nRF
//! `mgmt` path). The frame's CRC still refuses corruption; what is
//! missing is "who", and the wrapper that has an identity store should
//! check `Verification` itself. Said plainly rather than hidden.

use alloc::vec::Vec;

use leviculum_core::Identity;
use leviculum_lxmf::{
    message::{DeliveryMethod, Verification},
    LxmfNode, Message,
};

use crate::device::{Params, Report};

/// This node's `lxmf.delivery` hash — the destination a remote
/// addresses. Same derivation the nRF boards register.
pub fn delivery_hash(identity: &Identity) -> Option<[u8; 16]> {
    let dest = LxmfNode::delivery_destination_without_inbox(identity.clone()).ok()?;
    Some(*dest.hash().as_bytes())
}

/// What an inbound delivery turned out to be.
pub enum Decoded<P> {
    /// Valid params, the sender's delivery hash, and what the signature
    /// check could say.
    ///
    /// `verification` is `Unverified` unless `decode_params` was given
    /// the sender's identity — a board without an identity store for
    /// the remote cannot check the signature, and the field says so
    /// honestly rather than implying a check that never ran.
    Params {
        params: P,
        source: [u8; 16],
        verification: Verification,
        /// The received message's timestamp — boards without a wall
        /// clock reuse it for their reply (host clients reject
        /// epoch-0 timestamps).
        timestamp: f64,
    },
    /// An LXMF message, but its content is not a valid `P` frame.
    ///
    /// There is no "wrong destination" case here on purpose: in the
    /// opportunistic representation the destination is implicit — the
    /// bytes only exist because NodeCore decrypted them to *our*
    /// registered destination. The check that matters happened there.
    NotParams,
    /// Not an LXMF message.
    BadFrame,
}

/// Parse an opportunistic LXMF delivery into `P`.
///
/// `signer`, when the caller has the sender's identity (an announce it
/// kept, an allow-list), is checked against the message signature;
/// with `None` the signature is not checked and the result carries
/// `Verification::Unverified`.
pub fn decode_params<P: Params>(
    on_air: &[u8],
    our_dest: &[u8; 16],
    signer: Option<&Identity>,
) -> Decoded<P> {
    let msg = match Message::unpack(on_air, Some(*our_dest), signer, DeliveryMethod::Opportunistic) {
        Ok(m) => m,
        Err(_) => return Decoded::BadFrame,
    };
    match P::decode(&msg.content) {
        Ok(params) => Decoded::Params {
            params,
            source: msg.source_hash,
            verification: msg.verification,
            timestamp: msg.timestamp,
        },
        Err(_) => Decoded::NotParams,
    }
}

/// Build the opportunistic on-air bytes of a report message.
///
/// `timestamp_s` is whatever clock the board has — uptime seconds on a
/// board without an RTC. Honest, and not sortable across reboots.
pub fn encode_report<R: Report>(
    identity: &Identity,
    our_dest: &[u8; 16],
    target: &[u8; 16],
    report: &R,
    timestamp_s: f64,
) -> Option<Vec<u8>> {
    let msg = Message::create(
        *target,
        *our_dest,
        identity,
        timestamp_s,
        Vec::new(),
        report.encode(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .ok()?;
    msg.on_air().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thermostat::{ThermostatParams, ThermostatReport};
    use rand_core::OsRng;

    #[test]
    fn params_roundtrip_through_lxmf() {
        let remote = Identity::generate(&mut OsRng);
        let board = Identity::generate(&mut OsRng);
        let remote_dest = delivery_hash(&remote).unwrap();
        let board_dest = delivery_hash(&board).unwrap();

        let p = ThermostatParams {
            setpoint_c: 21.5,
            hysteresis_c: 0.3,
            sample_ms: 5000,
            report_every: 2,
        };
        let msg = Message::create(
            board_dest,
            remote_dest,
            &remote,
            1.0,
            Vec::new(),
            p.encode(),
            Vec::new(),
            DeliveryMethod::Opportunistic,
        )
        .unwrap();
        let on_air = msg.on_air().unwrap();

        match decode_params::<ThermostatParams>(&on_air, &board_dest, None) {
            Decoded::Params {
                params,
                source,
                verification,
                timestamp,
            } => {
                assert_eq!(params, p);
                assert_eq!(source, remote_dest);
                assert_eq!(verification, Verification::Unverified);
                assert_eq!(timestamp, 1.0);
            }
            _ => panic!("should decode"),
        }
        // With the sender's identity supplied, the signature verifies.
        match decode_params::<ThermostatParams>(&on_air, &board_dest, Some(&remote)) {
            Decoded::Params { verification, .. } => {
                assert_eq!(verification, Verification::Valid);
            }
            _ => panic!("should decode"),
        }
        // A real message whose content is not a THRM frame.
        let other = Message::create(
            board_dest,
            remote_dest,
            &remote,
            2.0,
            Vec::new(),
            b"hello".to_vec(),
            Vec::new(),
            DeliveryMethod::Opportunistic,
        )
        .unwrap();
        assert!(matches!(
            decode_params::<ThermostatParams>(&other.on_air().unwrap(), &board_dest, None),
            Decoded::NotParams
        ));
        assert!(matches!(
            decode_params::<ThermostatParams>(b"garbage", &board_dest, None),
            Decoded::BadFrame
        ));
    }

    #[test]
    fn report_is_a_signed_message() {
        let board = Identity::generate(&mut OsRng);
        let dest = delivery_hash(&board).unwrap();
        let target = [7u8; 16];
        let r = ThermostatReport {
            temperature_c: 20.0,
            setpoint_c: 21.0,
            on: true,
            sensor_lost: false,
            uptime_s: 42,
        };
        let on_air = encode_report(&board, &dest, &target, &r, 12.0).unwrap();
        let msg = Message::unpack(
            &on_air,
            Some(target),
            Some(&board),
            DeliveryMethod::Opportunistic,
        )
        .unwrap();
        assert_eq!(msg.source_hash, dest);
        assert_eq!(msg.content, r.encode());
        assert_eq!(
            msg.verification,
            leviculum_lxmf::message::Verification::Valid
        );
    }
}
