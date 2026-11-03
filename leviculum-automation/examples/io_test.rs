//! `io-test` host simulation — drives `App<IoTest>` against fake
//! digital/analog banks and shows the whole path without hardware:
//!
//! 1. DI/AI banks sampled, DO bank driven (mirror mode, then remote
//!    mask applied through a real LXMF params message);
//! 2. `IOTS` reports decoded on the way out — what a remote node sees;
//! 3. a config blob saved and re-booted — the flash path.
//!
//! Run: `cargo run -p leviculum-automation --example io_test`
//!
//! Everything under `mod fake` is what a board wrapper provides:
//! `DigitalIn`/`DigitalOut`/`AnalogIn` over its pins, a `ConfigStore`,
//! a `Clock`. On real hardware the three fake banks become three impls
//! and nothing else changes.

use leviculum_automation::app::Inbound;
use leviculum_automation::hal::{AnalogIn, Clock, ConfigStore, DigitalIn, DigitalOut, Log};
use leviculum_automation::iotest::{IoBus, IoTest, IoTestParams, IoTestReport, FLAG_MIRROR};
use leviculum_automation::{App, Device, Params};
use leviculum_core::Identity;
use leviculum_lxmf::message::DeliveryMethod;
use leviculum_lxmf::Message;
use rand_core::OsRng;

mod fake {
    use super::*;

    /// Eight switches as a byte of state — the outside world.
    pub struct DiBank {
        /// What the switches are set to.
        pub din: u8,
        /// When true every `read` fails — simulates a dead input rail.
        pub broken: bool,
    }

    impl DigitalIn for DiBank {
        fn read(&mut self, ch: u8) -> Option<bool> {
            if self.broken || ch >= 8 {
                return None;
            }
            Some(self.din >> ch & 1 == 1)
        }
    }

    /// Eight relays — the observable driven state.
    pub struct DoBank {
        pub dout: u8,
    }

    impl DigitalOut for DoBank {
        fn set(&mut self, ch: u8, on: bool) {
            if ch >= 8 {
                return;
            }
            if on {
                self.dout |= 1 << ch;
            } else {
                self.dout &= !(1 << ch);
            }
        }
    }

    /// Four pots as a slow sawtooth, channel 3 dead on purpose.
    pub struct AdcBank(pub u32);

    impl AnalogIn for AdcBank {
        fn read_counts(&mut self, ch: u8) -> Option<u16> {
            if ch >= 4 {
                return None;
            }
            if ch == 3 {
                return None; // broken channel shows u16::MAX in reports
            }
            Some(((self.0 * 137 + ch as u32 * 977) % 4096) as u16)
        }
    }

    pub struct RamStore(pub Option<Vec<u8>>);
    impl ConfigStore for RamStore {
        fn load(&mut self) -> Option<Vec<u8>> {
            self.0.clone()
        }
        fn save(&mut self, blob: &[u8]) -> bool {
            self.0 = Some(blob.to_vec());
            true
        }
    }

    /// A clock the test script advances by hand.
    pub struct HandClock(pub u64);
    impl Clock for HandClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    pub struct PrintLog;
    impl Log for PrintLog {
        fn line(&mut self, tag: &str, args: core::fmt::Arguments<'_>) {
            println!("{tag}{args}");
        }
    }
}

fn main() {
    use fake::*;

    let mut di = DiBank {
        din: 0,
        broken: false,
    };
    let mut do_ = DoBank { dout: 0 };
    let mut adc = AdcBank(0);
    let mut store = RamStore(None);
    let mut clock = HandClock(0);
    let mut log = PrintLog;

    // The board's LXMF address and a "remote" node — the remote signs
    // params; its delivery hash becomes the board's report target.
    let board_id = Identity::generate(&mut OsRng);
    let remote_id = Identity::generate(&mut OsRng);
    let our_dest = leviculum_automation::lxmf::delivery_hash(&board_id).unwrap();
    let remote_dest = leviculum_automation::lxmf::delivery_hash(&remote_id).unwrap();

    let mut app = App::boot(IoTest::new(), &mut store, 0, &mut log);
    println!("[sim] board dest={}", hex(&our_dest));

    // --- 1. mirror mode: switch pattern follows into dout + report ---
    println!("\n== mirror mode (dout follows din) ==");
    for (i, din) in [0b0000_0001u8, 0b0000_0110, 0b1010_0101].iter().enumerate() {
        di.din = *din;
        adc.0 += 7;
        clock.0 += 1_000; // DEFAULT sample_ms=500 — past it every loop
        let _tick = app.tick(
            &mut IoBus {
                din: &mut di,
                dout: &mut do_,
                adc: &mut adc,
            },
            &clock,
            &mut log,
        );
        println!("[sim] t{i} din={:08b} -> dout={:08b}", di.din, do_.dout);
        // report_every=20 in DEFAULT, so ask the device directly here.
        let r = app.device().report(clock.0);
        print_report(&r);
    }

    // --- 2. remote params over a real LXMF opportunistic message -----
    println!("\n== remote params via LXMF (mirror off, mask=0xF0) ==");
    let new_params = IoTestParams {
        flags: 0, // remote mode
        dout_mask: 0xF0,
        sample_ms: 200,
        report_every: 5,
    };
    let msg = Message::create(
        our_dest,
        remote_dest,
        &remote_id,
        1.0,
        Vec::new(),
        new_params.encode(),
        Vec::new(),
        DeliveryMethod::Opportunistic,
    )
    .expect("build params message");
    match app.on_message(&msg.on_air().unwrap(), &our_dest, clock.0, &mut log, Some(&remote_id)) {
        Inbound::Applied {
            blob,
            source,
            verification,
        } => {
            assert_eq!(source, remote_dest);
            println!(
                "[sim] params applied from={} verified={verification:?}",
                hex(&source[..4])
            );
            store.save(&blob);
            println!("[sim] blob persisted ({} bytes)", blob.len());
        }
        _ => unreachable!(),
    }

    // A remote-mask step: outputs come from dout_mask, not din.
    di.din = 0b0000_1111;
    adc.0 += 7;
    clock.0 += 1_000;
    app.tick(
        &mut IoBus {
            din: &mut di,
            dout: &mut do_,
            adc: &mut adc,
        },
        &clock,
        &mut log,
    );
    println!(
        "[sim] remote mode: din={:08b} dout={:08b} (mask wins)",
        di.din, do_.dout
    );
    assert_eq!(do_.dout, 0xF0);

    // --- 3. broken DI rail → input_invalid + flags in the report -----
    println!("\n== dead input rail ==");
    di.broken = true;
    adc.0 += 7;
    clock.0 += 1_000;
    app.tick(
        &mut IoBus {
            din: &mut di,
            dout: &mut do_,
            adc: &mut adc,
        },
        &clock,
        &mut log,
    );
    let r = app.device().report(clock.0);
    print_report(&r);

    // --- 4. re-boot from the persisted blob --------------------------
    println!("\n== reboot with persisted config ==");
    let app2 = App::boot(IoTest::new(), &mut store, 0, &mut log);
    let p = app2.device().params();
    println!(
        "[sim] after reboot: flags={:#04x} dout_mask={:#04x} sample_ms={} target={}",
        p.flags,
        p.dout_mask,
        p.sample_ms,
        app2.target().map(|t| hex(&t[..4])).unwrap_or_default()
    );
    assert_eq!(p.flags & FLAG_MIRROR, 0);
    assert_eq!(app2.target(), Some(remote_dest));

    println!("\n[sim] done — all paths exercised.");
}

fn print_report(r: &IoTestReport) {
    let adc: Vec<String> = r
        .adc
        .iter()
        .map(|c| {
            if *c == u16::MAX {
                "----".to_string()
            } else {
                format!("{c:4}")
            }
        })
        .collect();
    println!(
        "  [IOTS] din={:08b} dout={:08b} flags={:#04x} adc=[{}] uptime={}s",
        r.din,
        r.dout,
        r.flags,
        adc.join(", "),
        r.uptime_s
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
