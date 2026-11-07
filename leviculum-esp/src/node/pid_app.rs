//! The PID app: a valve regulator with a DS18B20 temperature input.
//!
//! A remote node retunes the loop with an LXMF message whose `content` is
//! a `PIDC` frame (and the sender becomes the report target); every sample
//! emits a signed LXMF status message with a `PIDS` frame back to it. The
//! debug port takes the text form of the same commands:
//!
//! ```text
//! PID SET kp=4 ki=0.05 kd=0 sp=22 sample_ms=2000 window_ms=10000
//!     any subset of keys; a good set applies, then persists to flash
//! PID GET            print the live config
//! PID IMPORT XXYY…   the PIDC frame as hex; decode or reject
//! PID REPORT         the PIDS status frame as hex
//! PID LXMF <hex>     a packed LXMF message as the remote node would send it
//! PID TARGET <hex>   set the report destination by hand
//! ```
//!
//! The DS18B20 sits on D1 (open-drain, external 4.7 kΩ pull-up), the valve
//! on D3. With no valid reading the valve fails closed.

use esp_hal::gpio::{DriveMode, Flex, InputConfig, Level, Output, OutputConfig, Pull};
use leviculum_core::node::NodeEvent;
use leviculum_core::DestinationHash;

use crate::node::app::{App, Ctx};
use crate::node::board::AppWiring;
use crate::{ds18b20, pid, pid_lxmf, store};

pub struct PidApp {
    our_dest: [u8; 16],
    pid: pid::Pid,
    temp_sensor: ds18b20::Ds18b20<'static>,
    valve: Output<'static>,
    report_target: Option<[u8; 16]>,
    status: pid::Status,
    duty: f32,
    last_t: Option<f32>,
    last_sample_ms: u64,
    next_sample: u64,
}

impl App for PidApp {
    fn start(ctx: &mut Ctx<'_>, w: AppWiring) -> Self {
        let mut ow_pin = Flex::new(w.onewire);
        ow_pin.apply_output_config(&OutputConfig::default().with_drive_mode(DriveMode::OpenDrain));
        // The internal pull-up is a backstop, not the 1-Wire pull-up: ~45 kΩ
        // is too weak for a long run, but it keeps a bare pad from reading a
        // steady low that the driver would take for a presence pulse.
        ow_pin.apply_input_config(&InputConfig::default().with_pull(Pull::Up));
        ow_pin.set_input_enable(true);
        ow_pin.set_output_enable(false);
        let mut temp_sensor = ds18b20::Ds18b20::new(ow_pin);
        ctx.log.line(
            "[DS18B20] ",
            if temp_sensor.present() {
                format_args!("state=present pin=D1")
            } else {
                format_args!("state=absent pin=D1 note=no-presence-pulse")
            },
        );

        let valve = Output::new(w.valve, Level::Low, OutputConfig::default());

        // The destination a remote node addresses: the LXMF delivery
        // destination of this node's identity, registered without an inbox
        // (the PID frames are read straight off `PacketReceived`).
        let our_dest = match pid_lxmf::delivery_hash(ctx.identity.clone()) {
            Some(h) => {
                let mut hex = [0u8; 32];
                to_hex(&h, &mut hex);
                ctx.log.line(
                    "[PID] ",
                    format_args!("dest={}", core::str::from_utf8(&hex).unwrap_or("")),
                );
                h
            }
            None => {
                ctx.log.line("[PID] ", format_args!("dest=unavailable"));
                [0u8; 16]
            }
        };
        if let Ok(dest) =
            leviculum_lxmf::LxmfNode::delivery_destination_without_inbox(ctx.identity.clone())
        {
            ctx.node.register_destination(dest);
        }

        // Config comes from flash when it was ever set — a remote node's
        // tune survives reset — else the default.
        let (cfg, report_target) = match ctx.with_flash(store::load_config) {
            Some((c, t)) => {
                ctx.log.line(
                    "[PID] ",
                    format_args!(
                        "state=loaded sp={} kp={} ki={} kd={} sample_ms={} window_ms={}",
                        c.setpoint_c, c.kp, c.ki, c.kd, c.sample_ms, c.window_ms
                    ),
                );
                (c, t)
            }
            None => {
                ctx.log.line(
                    "[PID] ",
                    format_args!(
                        "state=default sp=22 kp=4 ki=0.05 kd=0 sample_ms=2000 window_ms=10000"
                    ),
                );
                (pid::Config::DEFAULT, None)
            }
        };

        PidApp {
            our_dest,
            pid: pid::Pid::new(cfg),
            temp_sensor,
            valve,
            report_target,
            status: pid::Status {
                temperature_c: 0.0,
                setpoint_c: cfg.setpoint_c,
                duty: 0.0,
                output_on: false,
                sensor_lost: true,
                uptime_s: 0,
            },
            duty: 0.0,
            last_t: None,
            last_sample_ms: 0,
            next_sample: 0,
        }
    }

    fn delivery_hash(&self) -> DestinationHash {
        DestinationHash::new(self.our_dest)
    }

    fn announce_data(&self) -> &'static [u8] {
        b"leviculum-xiao_s3-pid"
    }

    /// The application half of a `TickOutput`: of the events only
    /// `PacketReceived` matters — a decrypted delivery to a registered
    /// destination. Ours is `lxmf.delivery`; its payload is an LXMF message
    /// whose `content` is a `PIDC` frame, handled by the same
    /// `decode_config_message` the debug port's `PID LXMF` feeds.
    fn on_events(&mut self, ctx: &mut Ctx<'_>, events: &[NodeEvent]) {
        for event in events {
            let NodeEvent::PacketReceived {
                destination, data, ..
            } = event
            else {
                continue;
            };
            if destination.as_bytes() != &self.our_dest {
                continue;
            }
            match pid_lxmf::decode_config_message(data, &self.our_dest) {
                pid_lxmf::Inbound::Config(cfg, src) => {
                    self.pid.set_config(cfg, crate::uptime_ms());
                    self.report_target = Some(src);
                    let saved = save_config(ctx, &cfg, Some(&src)).is_ok();
                    let mut shex = [0u8; 32];
                    to_hex(&src, &mut shex);
                    ctx.log.line(
                        "[PID] ",
                        format_args!(
                            "cmd=mesh result=ok from={} persisted={}",
                            core::str::from_utf8(&shex).unwrap_or(""),
                            saved
                        ),
                    );
                }
                pid_lxmf::Inbound::NotOurs => {
                    ctx.log
                        .line("[PID] ", format_args!("cmd=mesh result=not-ours"));
                }
                pid_lxmf::Inbound::BadFrame => {
                    ctx.log
                        .line("[PID] ", format_args!("cmd=mesh result=bad-frame"));
                }
            }
        }
    }

    fn on_line(&mut self, ctx: &mut Ctx<'_>, line: &str) {
        self.handle_line(ctx, line.trim());
    }

    fn tick(&mut self, ctx: &mut Ctx<'_>, now: u64) {
        if now >= self.next_sample {
            let dt = if self.last_sample_ms == 0 {
                self.pid.config().sample_ms as f32 / 1000.0
            } else {
                (now - self.last_sample_ms) as f32 / 1000.0
            };
            self.last_sample_ms = now;
            self.next_sample = now + self.pid.config().sample_ms as u64;

            match self.temp_sensor.measure() {
                Some(t) => {
                    self.last_t = Some(t);
                    self.duty = self.pid.update(t, dt);
                }
                None => {
                    // Sensor lost: fail the valve closed, flag it. A stuck
                    // open valve on a blind controller is the failure this
                    // refuses.
                    self.duty = 0.0;
                }
            }
            self.status = pid::Status {
                temperature_c: self.last_t.unwrap_or(0.0),
                setpoint_c: self.pid.config().setpoint_c,
                duty: self.duty,
                output_on: self.pid.valve_on(self.duty, now),
                sensor_lost: self.last_t.is_none(),
                uptime_s: (now / 1000) as u32,
            };
            self.valve.set_level(if self.status.output_on {
                Level::High
            } else {
                Level::Low
            });
            ctx.log.line(
                "[PID] ",
                format_args!(
                    "t={} sp={} duty={:.3} out={} {}",
                    self.status.temperature_c,
                    self.status.setpoint_c,
                    self.status.duty,
                    if self.status.output_on { "on" } else { "off" },
                    if self.status.sensor_lost {
                        "sensor=lost"
                    } else {
                        ""
                    },
                ),
            );
            self.report(ctx, now);
        } else {
            // Between samples the valve still needs its window slicing.
            self.valve.set_level(if self.pid.valve_on(self.duty, now) {
                Level::High
            } else {
                Level::Low
            });
        }
    }
}

impl PidApp {
    /// The LXMF status report — `content = PIDS`, on-air opportunistic
    /// bytes handed to the node, which encrypts and radiates them. The
    /// debug port still sees the bytes when the radio is down.
    fn report(&mut self, ctx: &mut Ctx<'_>, now: u64) {
        let Some(target) = self.report_target else {
            return;
        };
        let Some(on_air) = pid_lxmf::build_status_message(
            ctx.identity,
            &self.our_dest,
            &target,
            &self.status,
            now as f64 / 1000.0,
        ) else {
            return;
        };
        if ctx.carrier.is_up() {
            match ctx
                .node
                .send_single_packet(&DestinationHash::new(target), &on_air)
            {
                Ok((_pkt_hash, out)) => ctx.dispatch(out),
                Err(_) => ctx
                    .log
                    .line("[PID] ", format_args!("lxmf=send-failed")),
            }
        } else {
            let mut mhex = [0u8; 1080];
            let hl = to_hex(&on_air, &mut mhex);
            ctx.log.line(
                "[PID] ",
                format_args!("lxmf={}", core::str::from_utf8(&mhex[..hl]).unwrap_or("")),
            );
        }
    }

    fn handle_line(&mut self, ctx: &mut Ctx<'_>, line: &str) {
        let mut it = line.split_whitespace();
        if it.next() != Some("PID") {
            return;
        }
        match it.next() {
            Some("SET") => {
                let mut cfg = *self.pid.config();
                let mut bad = false;
                for kv in it {
                    let Some((k, v)) = kv.split_once('=') else {
                        bad = true;
                        break;
                    };
                    let f = |s: &str| s.parse::<f32>();
                    let u = |s: &str| s.parse::<u32>();
                    let ok = match k {
                        "kp" => f(v).map(|x| cfg.kp = x).is_ok(),
                        "ki" => f(v).map(|x| cfg.ki = x).is_ok(),
                        "kd" => f(v).map(|x| cfg.kd = x).is_ok(),
                        "sp" => f(v).map(|x| cfg.setpoint_c = x).is_ok(),
                        "sample_ms" => u(v).map(|x| cfg.sample_ms = x).is_ok(),
                        "window_ms" => u(v).map(|x| cfg.window_ms = x).is_ok(),
                        _ => false,
                    };
                    if !ok {
                        bad = true;
                        break;
                    }
                }
                if bad || cfg.sample_ms == 0 || cfg.window_ms == 0 {
                    ctx.log
                        .line("[PID] ", format_args!("cmd=set result=reject"));
                    return;
                }
                self.pid.set_config(cfg, crate::uptime_ms());
                match save_config(ctx, &cfg, self.report_target.as_ref()) {
                    Ok(()) => ctx.log.line(
                        "[PID] ",
                        format_args!(
                            "cmd=set result=ok persisted=yes sp={} kp={} ki={} kd={}",
                            cfg.setpoint_c, cfg.kp, cfg.ki, cfg.kd
                        ),
                    ),
                    Err(_) => ctx
                        .log
                        .line("[PID] ", format_args!("cmd=set result=ok persisted=no")),
                }
            }
            Some("GET") => {
                let c = self.pid.config();
                ctx.log.line(
                    "[PID] ",
                    format_args!(
                        "sp={} kp={} ki={} kd={} sample_ms={} window_ms={}",
                        c.setpoint_c, c.kp, c.ki, c.kd, c.sample_ms, c.window_ms
                    ),
                );
            }
            Some("IMPORT") => {
                let hex = it.next().unwrap_or("");
                let mut frame = [0u8; pid::CONFIG_FRAME_LEN];
                if unhex(hex, &mut frame) != pid::CONFIG_FRAME_LEN {
                    ctx.log
                        .line("[PID] ", format_args!("cmd=import result=bad-hex"));
                    return;
                }
                match pid::Config::decode(&frame) {
                    Ok(cfg) if cfg.sample_ms > 0 && cfg.window_ms > 0 => {
                        self.pid.set_config(cfg, crate::uptime_ms());
                        let saved =
                            save_config(ctx, &cfg, self.report_target.as_ref())
                                .is_ok();
                        ctx.log.line(
                            "[PID] ",
                            format_args!("cmd=import result=ok persisted={}", saved),
                        );
                    }
                    _ => ctx
                        .log
                        .line("[PID] ", format_args!("cmd=import result=reject")),
                }
            }
            Some("REPORT") => {
                let frame = self.status.encode();
                let mut hex = [0u8; pid::STATUS_FRAME_LEN * 2];
                let _ = to_hex(&frame, &mut hex);
                ctx.log.line(
                    "[PID] ",
                    format_args!("frame={}", core::str::from_utf8(&hex).unwrap_or("")),
                );
            }
            Some("LXMF") => {
                // A packed LXMF message as the remote node would radiate
                // it. Same decode the transport uses.
                let hex = it.next().unwrap_or("");
                let mut data = [0u8; 512];
                let n = unhex(hex, &mut data);
                match pid_lxmf::decode_config_message(&data[..n], &self.our_dest) {
                    pid_lxmf::Inbound::Config(cfg, src) => {
                        self.pid.set_config(cfg, crate::uptime_ms());
                        self.report_target = Some(src);
                        let saved = save_config(ctx, &cfg, Some(&src)).is_ok();
                        let mut shex = [0u8; 32];
                        to_hex(&src, &mut shex);
                        ctx.log.line(
                            "[PID] ",
                            format_args!(
                                "cmd=lxmf result=ok target={} persisted={}",
                                core::str::from_utf8(&shex).unwrap_or(""),
                                saved
                            ),
                        );
                    }
                    pid_lxmf::Inbound::NotOurs => {
                        ctx.log
                            .line("[PID] ", format_args!("cmd=lxmf result=not-ours"));
                    }
                    pid_lxmf::Inbound::BadFrame => {
                        ctx.log
                            .line("[PID] ", format_args!("cmd=lxmf result=bad-frame"));
                    }
                }
            }
            Some("TARGET") => {
                let mut t = [0u8; 16];
                if unhex(it.next().unwrap_or(""), &mut t) == 16 {
                    self.report_target = Some(t);
                    let cfg = *self.pid.config();
                    let saved = save_config(ctx, &cfg, Some(&t)).is_ok();
                    ctx.log.line(
                        "[PID] ",
                        format_args!("cmd=target result=ok persisted={saved}"),
                    );
                } else {
                    ctx.log
                        .line("[PID] ", format_args!("cmd=target result=bad-hash"));
                }
            }
            _ => {}
        }
    }
}

/// hex → bytes; returns bytes written.
fn unhex(s: &str, out: &mut [u8]) -> usize {
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut n = 0;
    while n * 2 + 1 < bytes.len() && n < out.len() {
        match (nib(bytes[n * 2]), nib(bytes[n * 2 + 1])) {
            (Some(h), Some(l)) => {
                out[n] = h << 4 | l;
                n += 1;
            }
            _ => break,
        }
    }
    n
}

fn to_hex(bytes: &[u8], out: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut n = 0;
    for &b in bytes {
        if n + 2 > out.len() {
            break;
        }
        out[n] = HEX[(b >> 4) as usize];
        out[n + 1] = HEX[(b & 0xf) as usize];
        n += 2;
    }
    n
}

/// Persist the PID config and report target through the shared flash.
fn save_config(
    ctx: &Ctx<'_>,
    cfg: &pid::Config,
    target: Option<&[u8; 16]>,
) -> Result<(), esp_storage::FlashStorageError> {
    ctx.with_flash(|f| store::save_config(f, cfg, target))
}
