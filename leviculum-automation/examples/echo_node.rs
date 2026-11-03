//! `echo` LNode host simulation — a remote node sends an `ECHC` params
//! frame carrying arbitrary bytes over a real, signed, opportunistic
//! LXMF message; the board applies it and the next `ECHS` report echoes
//! the same bytes back.
//!
//! Run: `cargo run -p leviculum-automation --example echo_node`
//!
//! This is the whole LNode-side path without a board: `App<Echo>` +
//! `NoIo` stand in for the mesh glue a firmware wrapper supplies; the
//! "remote" is a second `Identity` whose messages are built exactly as
//! `lxmf`/`thermostat_host` build them.

use leviculum_automation::app::{Inbound, NoIo};
use leviculum_automation::echo::{Echo, EchoParams, EchoReport};
use leviculum_automation::hal::{Clock, ConfigStore, Log};
use leviculum_automation::{App, Params, Report};
use leviculum_core::Identity;
use leviculum_lxmf::message::DeliveryMethod;
use leviculum_lxmf::Message;
use rand_core::OsRng;

struct RamStore(Option<Vec<u8>>);
impl ConfigStore for RamStore {
    fn load(&mut self) -> Option<Vec<u8>> {
        self.0.clone()
    }
    fn save(&mut self, blob: &[u8]) -> bool {
        self.0 = Some(blob.to_vec());
        true
    }
}

struct HandClock(u64);
impl Clock for HandClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

struct PrintLog;
impl Log for PrintLog {
    fn line(&mut self, tag: &str, args: core::fmt::Arguments<'_>) {
        println!("{tag}{args}");
    }
}

/// Remote side: wrap `payload` in an `ECHC` frame, then in a signed
/// opportunistic LXMF message to the board — the exact bytes a real
/// sender puts on the mesh.
fn send_params(remote: &Identity, remote_dest: [u8; 16], board_dest: [u8; 16], payload: &[u8]) -> Vec<u8> {
    Message::create(
        board_dest,
        remote_dest,
        remote,
        1.0,
        Vec::new(),
        EchoParams::of(payload).encode(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .expect("params message")
    .on_air()
    .expect("on-air")
}

fn main() {
    let board_id = Identity::generate(&mut OsRng);
    let remote_id = Identity::generate(&mut OsRng);
    let board_dest = leviculum_automation::lxmf::delivery_hash(&board_id).unwrap();
    let remote_dest = leviculum_automation::lxmf::delivery_hash(&remote_id).unwrap();

    println!("[echo] board={}", hex(&board_dest[..8]));
    println!("[echo] remote={}", hex(&remote_dest[..8]));

    let mut store = RamStore(None);
    let mut clock = HandClock(0);
    let mut log = PrintLog;
    let mut app = App::boot(Echo::new(), &mut store, 0, &mut log);

    for payload in [b"ping".as_slice(), b"hello leviculum".as_slice()] {
        println!("\n== remote sends {:?} ==", String::from_utf8_lossy(payload));

        // 1. inbound: LXMF message → params → apply (and learn target).
        let on_air = send_params(&remote_id, remote_dest, board_dest, payload);
        match app.on_message(&on_air, &board_dest, clock.0, &mut log, Some(&remote_id)) {
            Inbound::Applied {
                blob,
                source,
                verification,
            } => {
                println!(
                    "[echo] applied from={} verified={verification:?}",
                    hex(&source[..4])
                );
                store.save(&blob);
            }
            _ => unreachable!(),
        }

        // 2. the board's next cycle: step produces the echo report.
        clock.0 += 1_500;
        let tick = app.tick(&mut NoIo, &clock, &mut log);
        let report = tick.report.expect("report_every=1");
        let on_air = app
            .report_message(&board_id, &board_dest, &report, clock.0)
            .expect("target learned from params");

        // 3. remote side: unpack the signed reply, decode ECHS.
        let msg = Message::unpack(
            &on_air,
            Some(remote_dest),
            Some(&board_id),
            DeliveryMethod::Opportunistic,
        )
        .expect("reply unpack");
        let echo = EchoReport::decode(&msg.content).expect("ECHS frame");
        println!(
            "[echo] remote got {:?} applied={} verified={:?}",
            String::from_utf8_lossy(echo.content()),
            echo.applied,
            msg.verification
        );
        assert_eq!(echo.content(), payload);
    }

    println!("\n[echo] done — params in, bytes out, signature verified both ways.");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
