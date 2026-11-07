//! Add-on sensors on the shared I2C pair: the BMV080 particulate sensor and
//! the BME690 environmental sensor.
//!
//! Both are add-ons on the XIAO kit (the Qwiic pads), so presence is a
//! runtime question and the answer is printed, not assumed — the same
//! `no-hardware` discipline the nRF boards apply to GNSS. Everything past
//! the ACK is behind a feature each: `bmv080-sdk` links Bosch's licensed
//! blob, `bme690` compiles Bosch's BSD-3 driver; without them the probe
//! line is the whole story for that sensor.

use alloc::boxed::Box;

use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::Blocking;

use crate::node::board::{Board, SensorWiring};
use crate::node::boot::{self, Booted};
use crate::{bme690, bmv080, UsbLog};

/// The sensors, and the bus they share.
pub struct Sensors {
    /// Boxed so its address is stable: the drivers hold a `SharedI2c`, a
    /// raw pointer to this bus, and this struct moves.
    _i2c: Box<I2c<'static, Blocking>>,
    #[cfg(feature = "bmv080-sdk")]
    bmv080: Option<bmv080::sdk::Sensor<'static>>,
    #[cfg(feature = "bmv080-sdk")]
    sink: bmv080::sdk::Measurement,
    #[cfg(feature = "bme690")]
    bme: Option<bme690::api::Sensor<'static>>,
}

/// Open the bus and ask each sensor whether it is there, once.
pub fn start<B: Board>(boot: &mut Booted, w: SensorWiring) -> Sensors {
    let mut i2c = match I2c::new(w.i2c, I2cConfig::default()) {
        Ok(i2c) => Box::new(i2c.with_sda(w.sda).with_scl(w.scl)),
        Err(_) => {
            boot.log
                .line("[BMV080] ", format_args!("state=down reason=i2c-config"));
            boot::halt_blinking::<B>(boot);
        }
    };

    // Both probes run first — before the bus is handed out — because
    // `SharedI2c::new` is the moment `i2c` stops being borrowable by name.
    let bmv080_present = bmv080::probe(&mut i2c) == bmv080::Probe::Present;
    let bme690_addr = bme690::probe(&mut i2c);

    // The bus is shared: BMV080 and BME690 both live on these two pins,
    // so it is handed out as a `SharedI2c` — one handle per driver,
    // exclusivity enforced by sequencing rather than by the type system
    // (see `SharedI2c` in lib.rs).
    #[cfg(any(feature = "bmv080-sdk", feature = "bme690"))]
    let bus = {
        // SAFETY of the lifetime: the box lives in the returned `Sensors`,
        // which lives for the rest of the program.
        let ptr: *mut I2c<'static, Blocking> = &mut *i2c;
        crate::SharedI2c::new(unsafe { &mut *ptr })
    };

    let log = &mut boot.log;

    #[cfg(feature = "bmv080-sdk")]
    let bmv080 = match bmv080_present {
        true => match bmv080::sdk::Sensor::open(bus) {
            Ok(mut sensor) => {
                let (maj, min, pat) = bmv080::sdk::Sensor::driver_version();
                let id = sensor.sensor_id().unwrap_or([b'?'; 13]);
                log.line(
                    "[BMV080] ",
                    format_args!(
                        "state=up drv={}.{}.{} id={}",
                        maj,
                        min,
                        pat,
                        core::str::from_utf8(&id).unwrap_or("?")
                    ),
                );
                // Duty cycling: 10 s integration window every 60 s — the
                // SDK's own defaults, sized for a battery node.
                if let Err(st) = sensor
                    .configure_duty_cycle(10.0, 60)
                    .and_then(|_| sensor.start_duty_cycling())
                {
                    log.line(
                        "[BMV080] ",
                        format_args!("state=down reason=start status={st}"),
                    );
                    None
                } else {
                    Some(sensor)
                }
            }
            Err(st) => {
                log.line(
                    "[BMV080] ",
                    format_args!("state=down reason=open status={st}"),
                );
                None
            }
        },
        false => {
            log.line(
                "[BMV080] ",
                format_args!(
                    "state=absent addr=0x{:02x} note=not-fitted-or-moved-jumper",
                    bmv080::I2C_ADDRESS
                ),
            );
            None
        }
    };
    #[cfg(not(feature = "bmv080-sdk"))]
    if bmv080_present {
        log.line(
            "[BMV080] ",
            format_args!(
                "state=present addr=0x{:02x} note=probe-only-no-sdk",
                bmv080::I2C_ADDRESS
            ),
        );
    } else {
        log.line(
            "[BMV080] ",
            format_args!(
                "state=absent addr=0x{:02x} note=not-fitted-or-moved-jumper",
                bmv080::I2C_ADDRESS
            ),
        );
    }

    // The BME690: an open-source driver, so `--features bme690` compiles
    // it rather than linking a blob. Forced mode, one sample per banner
    // tick.
    #[cfg(feature = "bme690")]
    let bme = match bme690_addr {
        Some(addr) => match bme690::api::Sensor::open(bus, addr) {
            Ok(mut sensor) => {
                let chip = sensor.chip_id();
                log.line(
                    "[BME690] ",
                    format_args!("state=up addr=0x{addr:02x} chip=0x{chip:02x}"),
                );
                Some(sensor)
            }
            Err(st) => {
                log.line(
                    "[BME690] ",
                    format_args!("state=down reason=init status={st}"),
                );
                None
            }
        },
        None => {
            log.line(
                "[BME690] ",
                format_args!("state=absent addr=0x76|0x77 note=not-fitted"),
            );
            None
        }
    };
    #[cfg(not(feature = "bme690"))]
    match bme690_addr {
        Some(addr) => log.line(
            "[BME690] ",
            format_args!("state=present addr=0x{addr:02x} note=probe-only-no-sdk"),
        ),
        None => log.line(
            "[BME690] ",
            format_args!("state=absent addr=0x76|0x77 note=not-fitted"),
        ),
    }

    Sensors {
        _i2c: i2c,
        #[cfg(feature = "bmv080-sdk")]
        bmv080,
        #[cfg(feature = "bmv080-sdk")]
        sink: bmv080::sdk::Measurement::default(),
        #[cfg(feature = "bme690")]
        bme,
    }
}

impl Sensors {
    /// Run every loop pass. The BMV080 SDK wants `serve_interrupt` at least
    /// once a second; it drains the event FIFO and is cheap when nothing is
    /// due.
    #[allow(unused_variables)]
    pub fn poll(&mut self, log: &mut UsbLog<'static>) {
        #[cfg(feature = "bmv080-sdk")]
        if let Some(sensor) = self.bmv080.as_mut() {
            let seq = self.sink.seq;
            match sensor.serve(&mut self.sink) {
                Ok(()) if self.sink.seq != seq => {
                    log.line(
                        "[BMV080] ",
                        format_args!(
                            "pm1={} pm2_5={} pm10={} obstructed={} outofrange={}",
                            self.sink.pm1_ug_m3,
                            self.sink.pm2_5_ug_m3,
                            self.sink.pm10_ug_m3,
                            self.sink.obstructed as u8,
                            self.sink.outside_range as u8,
                        ),
                    );
                }
                Ok(()) => {}
                Err(st) => {
                    log.line(
                        "[BMV080] ",
                        format_args!("state=err reason=serve status={st}"),
                    );
                }
            }
        }
    }

    /// Run once per banner tick: one forced-mode BME690 sample, a 5 s
    /// cadence the sensor's 100 ms heater soaks in easily.
    #[allow(unused_variables)]
    pub fn banner(&mut self, log: &mut UsbLog<'static>) {
        #[cfg(feature = "bme690")]
        if let Some(sensor) = self.bme.as_mut() {
            match sensor.measure() {
                Ok(Some(m)) => {
                    log.line(
                        "[BME690] ",
                        format_args!(
                            "t={} p={} h={} gas={}",
                            m.temperature_c, m.pressure_pa, m.humidity_pct, m.gas_ohm,
                        ),
                    );
                }
                Ok(None) => {}
                Err(st) => {
                    log.line(
                        "[BME690] ",
                        format_args!("state=err reason=measure status={st}"),
                    );
                }
            }
        }
    }
}
