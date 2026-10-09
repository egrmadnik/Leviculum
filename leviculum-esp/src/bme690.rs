//! Bosch BME690 environmental sensor — temperature, pressure, humidity,
//! gas resistance.
//!
//! Same add-on story as [`crate::bmv080`]: the BME690 arrives on the
//! kit's Qwiic/I2C pair when it is fitted, so presence is a runtime
//! answer, not a board fact.
//!
//! Unlike the BMV080, the whole driver is **source**: Bosch publishes
//! `bme69x.c`/`bme69x_defs.h` under BSD-3 as the "BME690 SensorAPI"
//! (<https://github.com/boschsensortec/BME690_SensorAPI>), so under the
//! `bme690` cargo feature `build.rs` compiles it with
//! `xtensa-esp32s3-elf-gcc` and links it in. No blob, no licensing wall —
//! and no register reimplementation, for the same reason the SX1262
//! sequences live in `leviculum-core`: one copy of the protocol.
//!
//! The wire protocol here is plain register I2C, not the BMV080's
//! header-word scheme: `read(reg, data, len)` is an address write
//! followed by a read phase (what esp-hal calls `write_read`), and
//! `write(reg, data, len)` is the register byte plus the payload.

use esp_hal::i2c::master::I2c;

/// The BME690's two I2C addresses — `0x76` when the module's SDO pad is
/// low, `0x77` when it is high (`BME69X_I2C_ADDR_LOW/HIGH`). Which one a
/// given breakout chose is a hardware decision the probe reads.
pub const I2C_ADDRESSES: [u8; 2] = [0x76, 0x77];

/// Ask the bus which BME690 address answers, if any.
///
/// One byte read per candidate; the first ACK wins. `None` means the
/// sensor is not fitted — same `absent` semantics as
/// [`bmv080::probe`](crate::bmv080::probe).
pub fn probe(i2c: &mut I2c<'_, esp_hal::Blocking>) -> Option<u8> {
    let mut scratch = [0u8; 1];
    I2C_ADDRESSES
        .iter()
        .copied()
        .find(|&addr| i2c.read(addr, &mut scratch).is_ok())
}

// ---------------------------------------------------------------------
// The SensorAPI boundary (feature `bme690`)
// ---------------------------------------------------------------------

/// Rust-facing wrapper over the SensorAPI (`bme69x`).
///
/// `struct bme69x_dev` stays opaque: the Rust side allocates a buffer
/// sized by `bme69x_glue_dev_size()`, and the `csrc/bme69x_glue.c` shim
/// fills the interface fields, so nothing here reproduces Bosch's
/// internal struct layout. Only the two small config/output structs the
/// API hands back by pointer are mirrored — `bme69x_conf`,
/// `bme69x_heatr_conf` and `bme69x_data` — and only the non-FPU shape of
/// the last (the C side is built without `BME69X_USE_FPU`, so the fields
/// are scaled integers: temperature ×100 °C, humidity ×1000 %).
#[cfg(feature = "bme690")]
pub mod api {
    extern crate alloc;
    use alloc::boxed::Box;

    use core::ffi::c_void;

    use esp_hal::delay::Delay;

    /// `int8_t` status; `BME69X_OK` = 0.
    pub type Status = i8;
    pub const OK: Status = 0;

    /// `BME69X_CHIP_ID` — what `bme69x_init` reads back.
    pub const CHIP_ID: u8 = 0x61;

    /// `BME69X_FORCED_MODE`.
    const FORCED_MODE: u8 = 1;

    // bme69x_defs.h enum/const values used below:
    const OS_16X: u8 = 5;
    const FILTER_OFF: u8 = 0;
    const ODR_NONE: u8 = 0;
    const ENABLE: u8 = 1;

    /// `struct bme69x_conf` — oversampling/filter block.
    #[repr(C)]
    struct Conf {
        os_hum: u8,
        os_temp: u8,
        os_pres: u8,
        filter: u8,
        odr: u8,
    }

    /// `struct bme69x_heatr_conf` — forced-mode heater profile.
    /// `heatr_*_prof`/`profile_len`/`shared_heatr_dur` are profile-table
    /// fields unused by forced mode; mirrored to keep the C layout.
    #[repr(C)]
    struct HeatrConf {
        enable: u8,
        heatr_temp: u16,
        heatr_dur: u16,
        heatr_temp_prof: *mut u16,
        heatr_dur_prof: *mut u16,
        profile_len: u8,
        shared_heatr_dur: u16,
    }

    /// `struct bme69x_data`, non-FPU shape: the C side is compiled
    /// without `BME69X_USE_FPU`, so the outputs are scaled integers.
    #[repr(C)]
    struct Data {
        status: u8,
        gas_index: u8,
        meas_index: u8,
        res_heat: u8,
        idac: u8,
        gas_wait: u8,
        /// °C ×100.
        temperature: i16,
        /// Pa.
        pressure: u32,
        /// %RH ×1000.
        humidity: u32,
        /// Ω.
        gas_resistance: u32,
        t_lin: i64,
    }

    type ReadFn = extern "C" fn(u8, *mut u8, u32, *mut c_void) -> i8;
    type WriteFn = extern "C" fn(u8, *const u8, u32, *mut c_void) -> i8;
    type DelayFn = extern "C" fn(u32, *mut c_void);

    extern "C" {
        fn bme69x_glue_dev_size() -> usize;
        fn bme69x_glue_bind(
            dev: *mut c_void,
            intf_ptr: *mut c_void,
            read: ReadFn,
            write: WriteFn,
            delay: DelayFn,
        );
        fn bme69x_glue_chip_id(dev: *const c_void) -> u8;

        fn bme69x_init(dev: *mut c_void) -> Status;
        fn bme69x_set_conf(conf: *mut Conf, dev: *mut c_void) -> Status;
        fn bme69x_set_heatr_conf(op_mode: u8, conf: *const HeatrConf, dev: *mut c_void) -> Status;
        fn bme69x_set_op_mode(op_mode: u8, dev: *mut c_void) -> Status;
        fn bme69x_get_meas_dur(op_mode: u8, conf: *mut Conf, dev: *mut c_void) -> u32;
        fn bme69x_get_data(
            op_mode: u8,
            data: *mut Data,
            n_data: *mut u8,
            dev: *mut c_void,
        ) -> Status;
    }

    /// Bytes of storage for the opaque `bme69x_dev` — an upper bound, not
    /// a guess at the layout: [`Sensor::open`] checks the C side's own
    /// `sizeof` against it and refuses if it ever grew past the cap
    /// (an SDK bump would otherwise silently write past the buffer).
    const DEV_CAP: usize = 512;

    /// The per-sensor context `intf_ptr` points at. Lives inside the
    /// `Box` so its address stays stable for the sensor's lifetime — the
    /// C library stores the pointer and hands it back to callbacks
    /// forever.
    struct Ctx<'d> {
        bus: crate::SharedI2c<'d>,
        addr: u8,
    }

    /// What one forced measurement hands back, in real units.
    #[derive(Clone, Copy)]
    pub struct Measurement {
        /// °C.
        pub temperature_c: f32,
        /// Pa.
        pub pressure_pa: f32,
        /// %RH.
        pub humidity_pct: f32,
        /// Gas heater resistance, Ω — the air-quality proxy.
        pub gas_ohm: f32,
    }

    /// One open BME690.
    pub struct Sensor<'d> {
        inner: Box<Inner<'d>>,
        conf: Conf,
    }

    #[repr(align(8))]
    struct DevStore([u8; DEV_CAP]);

    struct Inner<'d> {
        ctx: Ctx<'d>,
        dev: DevStore,
    }

    impl<'d> Sensor<'d> {
        /// Bind the interface, `bme69x_init` (which reads and checks the
        /// chip ID — a wrong part at a right address still fails here),
        /// then apply the forced-mode configuration: ×16 oversampling,
        /// filter off, gas heater 300 °C / 100 ms — the values the SDK's
        /// own `forced_mode` example uses.
        pub fn open(bus: crate::SharedI2c<'d>, addr: u8) -> Result<Self, Status> {
            let mut inner = Box::new(Inner {
                ctx: Ctx { bus, addr },
                dev: DevStore([0; DEV_CAP]),
            });
            let dev_ptr = inner.dev.0.as_mut_ptr() as *mut c_void;
            // The storage cap is our number; the truth is C's. If an SDK
            // bump ever grows the struct past it, fail loudly here rather
            // than let init write past the end.
            let dev_size = unsafe { bme69x_glue_dev_size() };
            if dev_size > DEV_CAP {
                return Err(-1);
            }
            unsafe {
                bme69x_glue_bind(
                    dev_ptr,
                    &mut inner.ctx as *mut Ctx<'d> as *mut c_void,
                    i2c_read,
                    i2c_write,
                    delay_us,
                );
            }
            let st = unsafe { bme69x_init(dev_ptr) };
            if st != OK {
                return Err(st);
            }

            let conf = Conf {
                os_hum: OS_16X,
                os_temp: OS_16X,
                os_pres: OS_16X,
                filter: FILTER_OFF,
                odr: ODR_NONE,
            };
            let mut this = Sensor { inner, conf };
            let dev_ptr = this.inner.dev.0.as_mut_ptr() as *mut c_void;
            let st = unsafe { bme69x_set_conf(&mut this.conf, dev_ptr) };
            if st != OK {
                return Err(st);
            }
            let heatr = HeatrConf {
                enable: ENABLE,
                heatr_temp: 300,
                heatr_dur: 100,
                heatr_temp_prof: core::ptr::null_mut(),
                heatr_dur_prof: core::ptr::null_mut(),
                profile_len: 0,
                shared_heatr_dur: 0,
            };
            let st = unsafe { bme69x_set_heatr_conf(FORCED_MODE, &heatr, dev_ptr) };
            if st != OK {
                return Err(st);
            }
            Ok(this)
        }

        /// The chip ID `bme69x_init` read (0x61 on a real BME690).
        pub fn chip_id(&mut self) -> u8 {
            unsafe { bme69x_glue_chip_id(self.inner.dev.0.as_mut_ptr() as *const c_void) }
        }

        /// One forced-mode sample: wake the sensor, wait the measurement
        /// window plus the heater soak the SDK reports, then pull the
        /// data registers. `Ok(None)` is a completed cycle with no fresh
        /// reading (the `new_data` status bit was not set) — transient,
        /// not a fault.
        pub fn measure(&mut self) -> Result<Option<Measurement>, Status> {
            let dev_ptr = self.inner.dev.0.as_mut_ptr() as *mut c_void;
            let st = unsafe { bme69x_set_op_mode(FORCED_MODE, dev_ptr) };
            if st != OK {
                return Err(st);
            }
            // The SDK's own arithmetic: measurement duration from the
            // configured oversampling + the 100 ms heater window.
            let dur_us =
                unsafe { bme69x_get_meas_dur(FORCED_MODE, &mut self.conf, dev_ptr) } + 100 * 1000;
            Delay::new().delay_micros(dur_us);

            let mut data = Data {
                status: 0,
                gas_index: 0,
                meas_index: 0,
                res_heat: 0,
                idac: 0,
                gas_wait: 0,
                temperature: 0,
                pressure: 0,
                humidity: 0,
                gas_resistance: 0,
                t_lin: 0,
            };
            let mut n = 0u8;
            let st = unsafe { bme69x_get_data(FORCED_MODE, &mut data, &mut n, dev_ptr) };
            if st != OK {
                return Err(st);
            }
            // NEW_DATA_MSK 0x80: the status byte's "this reading is new"
            // bit; without it the frame is a stale repeat.
            if n == 0 || data.status & 0x80 == 0 {
                return Ok(None);
            }
            Ok(Some(Measurement {
                temperature_c: data.temperature as f32 / 100.0,
                pressure_pa: data.pressure as f32,
                humidity_pct: data.humidity as f32 / 1000.0,
                gas_ohm: data.gas_resistance as f32,
            }))
        }
    }

    // ---- the callbacks the driver calls ------------------------------
    //
    // `intf_ptr` is `*mut Ctx` — set once by `bme69x_glue_bind`, stable
    // because the Ctx lives in a boxed Inner. Same threading contract as
    // the BMV080 callbacks: synchronous, single-threaded, never unwind.

    /// `bme69x_read_fptr_t`: register write followed by a read phase —
    /// esp-hal's `write_read` is exactly that sequence.
    extern "C" fn i2c_read(reg_addr: u8, data: *mut u8, length: u32, intf: *mut c_void) -> i8 {
        let ctx = unsafe { &mut *(intf as *mut Ctx) };
        let i2c = unsafe { &mut *ctx.bus.as_sercom() };
        let buf = unsafe { core::slice::from_raw_parts_mut(data, length as usize) };
        match i2c.write_read(ctx.addr, &[reg_addr], buf) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    /// `bme69x_write_fptr_t`: register byte, then the payload, one
    /// transaction.
    extern "C" fn i2c_write(reg_addr: u8, data: *const u8, length: u32, intf: *mut c_void) -> i8 {
        let ctx = unsafe { &mut *(intf as *mut Ctx) };
        let i2c = unsafe { &mut *ctx.bus.as_sercom() };
        let mut buf = alloc::vec::Vec::with_capacity(1 + length as usize);
        buf.push(reg_addr);
        buf.extend_from_slice(unsafe { core::slice::from_raw_parts(data, length as usize) });
        match i2c.write(ctx.addr, &buf) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }

    /// `bme69x_delay_us_fptr_t`.
    extern "C" fn delay_us(us: u32, _intf: *mut c_void) {
        Delay::new().delay_micros(us);
    }
}
