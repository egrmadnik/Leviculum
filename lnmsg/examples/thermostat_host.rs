//! Remote host for an LNode thermostat: set its parameters over LXMF,
//! then sit and print the status reports it sends back.
//!
//! The LNode side is `leviculum_automation::thermostat` running in
//! `xiao_s3_auto`. This side is a plain Reticulum client attached to a
//! running shared instance (`lnsd` or `rnsd`), using `lnmsg`'s engine —
//! so the message plumbing is the same code `lnmsg send` uses; what this
//! example adds is the two frame types and a loop.
//!
//! ```text
//! lnsd running with the serial interface to a transport LNode
//!
//! $ cargo run -p lnmsg --example thermostat_host -- \
//!       --to 00b5bfe4fb2e3098a16698b069bd242d \
//!       --setpoint 21.5 --hysteresis 0.3 --sample-ms 10000 --report-every 3
//! [host] our address  6f1a…   (the board will report to this)
//! [host] resolving 00b5bfe4…
//! [host] params queued id=…  (sp=21.5 hyst=0.3 sample=10000ms report_every=3)
//! [host] waiting for reports (Ctrl-C to stop)
//! [THRS] t=20.75 sp=21.5 on=true lost=false uptime=312s  from=00b5bfe4…
//! ```
//!
//! How the two directions work:
//!
//! - **Params out**: a `THRM` frame goes as the *body* of a direct LXMF
//!   message to the board's `lxmf.delivery` hash. The engine resolves
//!   the path and the identity first (`Command::Resolve`) — a message
//!   cannot be encrypted to a peer whose key we have never seen.
//! - **Reports in**: the board remembers the *sender* of the accepted
//!   params as its report target, so reports come to this process's
//!   own delivery destination — `OutboxEvent::Received` with a `THRS`
//!   body. `verified` is whether the board's signature checked against
//!   the identity its announce carried.
//!
//! The identity is persisted under `${LNMSG_HOME}` like `lnmsg`'s own,
//! so this host's address is stable across runs — which is what lets
//! the board keep reporting to it after a reboot of either side.

use std::time::{Duration, Instant};

use clap::Parser;
use leviculum_automation::thermostat::{ThermostatParams, ThermostatReport};
use leviculum_automation::{Params, Report};
use lnmsg::engine::{attach, AttachConfig};
use lnmsg::outbox::{Command, Outbox, OutboxEvent, SendRequest, Via};

#[derive(Parser)]
#[command(about = "Set an LNode thermostat's parameters and print its reports")]
struct Args {
    /// The board's `lxmf.delivery` hash (32 hex) — `[AUTO] dest=…` on its
    /// debug port, or the announce in `lnomad`.
    #[arg(long)]
    to: String,
    #[arg(long, default_value_t = 21.0)]
    setpoint: f32,
    #[arg(long, default_value_t = 0.5)]
    hysteresis: f32,
    #[arg(long, default_value_t = 10_000)]
    sample_ms: u32,
    #[arg(long, default_value_t = 6)]
    report_every: u16,
    /// Shared instance name — the daemon to attach to.
    #[arg(long, default_value = "default")]
    instance: String,
    /// Only listen; do not send parameters.
    #[arg(long)]
    listen_only: bool,
    /// Give up resolving the board after this many seconds.
    #[arg(long, default_value_t = 120)]
    resolve_timeout_s: u64,
    /// Bench mode: do not attach to a daemon. Print the signed
    /// opportunistic LXMF params message as hex — paste it into the
    /// board's debug port as `AUTO LXMF <hex>`.
    #[arg(long)]
    bench_hex: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let board = lnmsg::address::parse(&args.to)?;

    let params = ThermostatParams {
        setpoint_c: args.setpoint,
        hysteresis_c: args.hysteresis,
        sample_ms: args.sample_ms,
        report_every: args.report_every,
    };
    if !params.valid() {
        return Err("parameters refused by ThermostatParams::valid (sample_ms >= 500, finite setpoint in -50..=150)".into());
    }

    // The same identity/home as `lnmsg`, so this host's delivery hash is
    // the one the board will keep reporting to.
    let home = lnmsg::identity::home_dir()?;
    let identity = lnmsg::identity::load_or_create(&home.join("identity"))?;

    if args.bench_hex {
        // The exact bytes the board's `App::on_message` consumes: an
        // opportunistic LXMF message from our delivery hash to the
        // board's, content = THRM frame, signed by our identity.
        let ours = lnmsg::engine::delivery_address(&identity)?;
        let msg = leviculum_lxmf::Message::create(
            board,
            ours,
            &identity,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs_f64(),
            Vec::new(),
            params.encode(),
            Vec::new(),
            leviculum_lxmf::message::DeliveryMethod::Opportunistic,
        )?;
        eprintln!(
            "[host] our address {}  (the board will set this as its report target)",
            hex(&ours)
        );
        println!("AUTO LXMF {}", hex(&msg.on_air()?));
        return Ok(());
    }
    let display_name = lnmsg::display_name::from_process(Some("thermostat-host"))?;

    let attached = attach(AttachConfig {
        instance: args.instance.clone(),
        storage_dir: home.join("thermostat-host"),
        identity,
        display_name: display_name.name.into_bytes(),
    })
    .await?;
    let mut outbox = attached.outbox;

    // 1. Wait for the engine to register our delivery destination.
    let our_address = loop {
        match next_event(&mut outbox).await? {
            OutboxEvent::Ready { address } => break address,
            OutboxEvent::Broken { detail } => return Err(detail.into()),
            _ => {}
        }
    };
    eprintln!(
        "[host] our address {}  (the board will report to this)",
        hex(&our_address)
    );

    if !args.listen_only {
        // 2. Resolve the board: path + identity.
        eprintln!("[host] resolving {}", hex(&board));
        outbox.submit(Command::Resolve { destination: board })?;
        let deadline = Instant::now() + Duration::from_secs(args.resolve_timeout_s);
        loop {
            if Instant::now() > deadline {
                return Err("board did not announce in time — is it powered and in range of a transport node?".into());
            }
            if let OutboxEvent::Resolved { destination } = next_event(&mut outbox).await? {
                if destination == board {
                    break;
                }
            }
        }

        // 3. Send the THRM frame as the body of a direct LXMF message.
        outbox.submit(Command::Send(Box::new(SendRequest {
            destination: board,
            title: b"thermostat params".to_vec(),
            body: params.encode(),
            via: Via::Direct,
        })))?;
        loop {
            match next_event(&mut outbox).await? {
                OutboxEvent::Queued { message_id } => {
                    eprintln!(
                        "[host] params queued id={}  (sp={} hyst={} sample={}ms report_every={})",
                        hex(&message_id[..8]),
                        params.setpoint_c,
                        params.hysteresis_c,
                        params.sample_ms,
                        params.report_every
                    );
                    break;
                }
                OutboxEvent::Refused { detail } => return Err(detail.into()),
                _ => {}
            }
        }
    }

    // 4. Print every THRS report that reaches us. Anything else that
    //    lands in our inbox is shown raw, so a mis-addressed test is
    //    visible rather than silently dropped.
    eprintln!("[host] waiting for reports (Ctrl-C to stop)");
    loop {
        match next_event(&mut outbox).await? {
            OutboxEvent::Received {
                source,
                body,
                verified,
                ..
            } => match ThermostatReport::decode(&body) {
                Ok(r) => println!(
                    "[THRS] t={:.2} sp={:.2} on={} lost={} uptime={}s  from={}{}",
                    r.temperature_c,
                    r.setpoint_c,
                    r.on,
                    r.sensor_lost,
                    r.uptime_s,
                    hex(&source[..4]),
                    if verified { "" } else { "  (unverified)" }
                ),
                Err(e) => println!(
                    "[other] from={} len={} not-a-report={e:?}",
                    hex(&source[..4]),
                    body.len()
                ),
            },
            OutboxEvent::State { message_id, state } => {
                eprintln!("[host] msg {} → {:?}", hex(&message_id[..8]), state);
            }
            OutboxEvent::Broken { detail } => return Err(detail.into()),
            _ => {}
        }
    }
}

/// The outbox is non-blocking by contract; poll it on a short tick.
async fn next_event(outbox: &mut impl Outbox) -> Result<OutboxEvent, Box<dyn std::error::Error>> {
    loop {
        if let Some(ev) = outbox.try_next_event()? {
            return Ok(ev);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
