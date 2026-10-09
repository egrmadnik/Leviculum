//! Bosch BMV080 particulate-matter sensor.
//!
//! The BMV080 measures PM1, PM2.5 and PM10 in free space. On the XIAO
//! ESP32-S3 kit it arrives as an add-on — the SparkFun breakout boards it
//! onto the Qwiic/I2C pair — so "is it there" is a runtime question, not
//! a board fact, exactly like the L76K's `no-hardware` answer on the nRF
//! boards.
//!
//! # Two levels of integration
//!
//! [`probe`] alone answers "does anything answer at the sensor's
//! address" — it proves the I2C peripheral, the two pins, the pull-ups on
//! the module and the address, and it needs nothing from Bosch.
//!
//! Everything past the ACK — the sensor handshake, the PM algorithm,
//! duty cycling — lives inside Bosch's pre-compiled `lib_bmv080.a`,
//! which is not vendored: it is a separately licensed download from
//! Bosch that `build.rs` links when the `bmv080-sdk` cargo feature is on
//! (see `Cargo.toml` for where the path comes from). The safe wrapper for
//! that library is [`sdk`], present only under the feature.

use esp_hal::i2c::master::I2c;

/// The BMV080's factory I2C address.
///
/// SparkFun's breakout adds four jumper-selectable alternates
/// (0x54–0x57); probing only the factory address is the deliberate
/// choice — a moved jumper is a bench decision, and the probe's job is
/// to answer "is the sensor we expect answering", not to guess where it
/// went.
pub const I2C_ADDRESS: u8 = 0x57;

/// What a probe found on the bus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The sensor acknowledged its address.
    Present,
    /// Nothing answered — not fitted, not powered, or a moved jumper.
    Absent,
}

/// Ask the bus whether a device answers at [`I2C_ADDRESS`].
///
/// A single one-byte read: a BMV080 that is present and powered ACKs the
/// address phase, and the read completes (the returned byte itself is
/// discarded — a register map is the SDK's business, not this probe's).
/// A NACK or a bus error both read as [`Probe::Absent`]: for the line
/// this feeds, "why nothing answered" is a bench question.
///
/// The call takes `&mut I2c` rather than owning it — the same bus may
/// carry an OLED or other add-ons, and a probe that seized the
/// peripheral would decide that for them.
pub fn probe(i2c: &mut I2c<'_, esp_hal::Blocking>) -> Probe {
    let mut scratch = [0u8; 1];
    match i2c.read(I2C_ADDRESS, &mut scratch) {
        Ok(()) => Probe::Present,
        Err(_) => Probe::Absent,
    }
}

// ---------------------------------------------------------------------
// The Bosch SDK boundary (feature `bmv080-sdk`)
// ---------------------------------------------------------------------

/// Safe-ish wrapper over Bosch's `lib_bmv080.a` (SDK v11.2.0,
/// `api/inc/bmv080.h` + `bmv080_defs.h`).
///
/// "Safe-ish" and not "safe": the library is a binary blob whose internal
/// behaviour we cannot inspect, and its callbacks thread a `void*` back
/// into our I2C driver. What this module does guarantee is the Rust side
/// of that contract: correct `repr(C)` layouts, a sercom pointer that is
/// valid for the sensor's lifetime, and callbacks that never unwind.
///
/// The wire protocol the callbacks implement is the one the SDK's own
/// `api_examples/xtensa_esp32/src/combridge.c` implements for ESP32:
/// every transfer is a 16-bit *header* word (register plus a R/W flag in
/// its MSB, dropped on the wire by a `<< 1` shift) followed by 16-bit
/// payload words, all big-endian.
#[cfg(feature = "bmv080-sdk")]
pub mod sdk {
    extern crate alloc;
    use alloc::vec::Vec;

    use core::ffi::c_void;

    use esp_hal::{delay::Delay, i2c::master::I2c, Blocking};

    use super::I2C_ADDRESS;

    /// `bmv080_status_code_t` — SDK status codes are an enum; the ones a
    /// caller can act on are named, the rest surface as a number.
    pub type Status = i32;
    pub const OK: Status = 0;

    /// `bmv080_handle_t`.
    type Handle = *mut c_void;
    /// `bmv080_sercom_handle_t` — ours is a `*mut I2c<Blocking>`; see
    /// [`Sensor::open`] for why that is sound.
    type SercomHandle = *mut c_void;

    type CallbackRead = extern "C" fn(SercomHandle, u16, *mut u16, u16) -> i8;
    type CallbackWrite = extern "C" fn(SercomHandle, u16, *const u16, u16) -> i8;
    type CallbackDelay = extern "C" fn(u32) -> i8;
    type CallbackTick = extern "C" fn() -> u32;
    type CallbackDataReady = extern "C" fn(Bmv080Output, *mut c_void);

    /// `bmv080_output_t` — `repr(C)`, same field order and sizes as
    /// `bmv080_defs.h`; the blob hands it to `data_ready` by value.
    #[repr(C)]
    pub struct Bmv080Output {
        /// Seconds since the measurement started.
        pub runtime_in_sec: f32,
        /// PM2.5 mass concentration, µg/m³.
        pub pm2_5_mass_concentration: f32,
        /// PM1 mass concentration, µg/m³.
        pub pm1_mass_concentration: f32,
        /// PM10 mass concentration, µg/m³.
        pub pm10_mass_concentration: f32,
        /// PM2.5 number concentration, particles/cm³.
        pub pm2_5_number_concentration: f32,
        /// PM1 number concentration, particles/cm³.
        pub pm1_number_concentration: f32,
        /// PM10 number concentration, particles/cm³.
        pub pm10_number_concentration: f32,
        /// Sensor is obstructed; the reading is not valid.
        pub is_obstructed: bool,
        /// PM2.5 outside the specified 0–1000 µg/m³ range.
        pub is_outside_measurement_range: bool,
        reserved_0: f32,
        reserved_1: f32,
        reserved_2: f32,
        extended_info: *mut c_void,
    }

    extern "C" {
        fn bmv080_open(
            handle: *mut Handle,
            sercom: SercomHandle,
            read: CallbackRead,
            write: CallbackWrite,
            delay: CallbackDelay,
        ) -> Status;
        fn bmv080_get_driver_version(
            major: *mut u16,
            minor: *mut u16,
            patch: *mut u16,
            git_hash: *mut u8, // char[12]
            num_commits_ahead: *mut i32,
        ) -> Status;
        fn bmv080_reset(handle: Handle) -> Status;
        fn bmv080_get_sensor_id(handle: Handle, id: *mut u8 /* char[13] */) -> Status;
        fn bmv080_set_parameter(
            handle: Handle,
            key: *const core::ffi::c_char,
            value: *const c_void,
        ) -> Status;
        fn bmv080_start_duty_cycling_measurement(
            handle: Handle,
            tick: CallbackTick,
            mode: i32,
        ) -> Status;
        fn bmv080_serve_interrupt(
            handle: Handle,
            data_ready: CallbackDataReady,
            callback_parameters: *mut c_void,
        ) -> Status;
        fn bmv080_stop_measurement(handle: Handle) -> Status;
        fn bmv080_close(handle: *mut Handle) -> Status;
    }

    /// The duty-cycling mode the SDK defines (`E_BMV080_DUTY_CYCLING_MODE_0`
    /// — fixed duty cycle, ON time = integration time). It is the only
    /// mode SDK v11.2.0 offers.
    const DUTY_CYCLING_MODE_0: i32 = 0;

    /// What a finished measurement period hands back. The fields the
    /// firmware reports; the rest of `bmv080_output_t` stays in the blob.
    ///
    /// `seq` is bumped by `data_ready` on every fill — the caller compares
    /// it to learn whether [`Sensor::serve`] produced a new reading.
    #[derive(Clone, Copy, Default)]
    pub struct Measurement {
        pub pm1_ug_m3: f32,
        pub pm2_5_ug_m3: f32,
        pub pm10_ug_m3: f32,
        pub obstructed: bool,
        pub outside_range: bool,
        pub seq: u32,
    }

    /// One open sensor unit.
    ///
    /// The SDK handle is created by [`Sensor::open`] and destroyed by
    /// [`Drop`] — an `Sensor` that exists is a handle the blob owns.
    pub struct Sensor<'d> {
        handle: Handle,
        /// The bus the SDK's callbacks reach through `sercom`. Held as a
        /// shareable handle — see [`crate::SharedI2c`].
        bus: crate::SharedI2c<'d>,
    }

    impl<'d> Sensor<'d> {
        /// Open the sensor unit: handshake over I2C, software reset, then
        /// the driver version and sensor ID are readable.
        ///
        /// `sercom_handle` handed to the blob is the bus's raw pointer —
        /// sound because every SDK call is synchronous on this thread and
        /// the callbacks reconstruct a `&mut I2c` from it only while
        /// inside a call.
        pub fn open(bus: crate::SharedI2c<'d>) -> Result<Self, Status> {
            let mut this = Sensor {
                handle: core::ptr::null_mut(),
                bus,
            };
            let sercom = this.bus.as_sercom() as SercomHandle;
            let st =
                unsafe { bmv080_open(&mut this.handle, sercom, i2c_read, i2c_write, delay_ms) };
            if st != OK {
                return Err(st);
            }
            Ok(this)
        }

        /// `bmv080_reset`: hardware + software reset.
        pub fn reset(&mut self) -> Result<(), Status> {
            let st = unsafe { bmv080_reset(self.handle) };
            if st != OK {
                return Err(st);
            }
            Ok(())
        }

        /// The sensor's ID string (`bmv080_get_sensor_id`, 13 chars).
        pub fn sensor_id(&mut self) -> Result<[u8; 13], Status> {
            let mut id = [0u8; 13];
            let st = unsafe { bmv080_get_sensor_id(self.handle, id.as_mut_ptr()) };
            if st != OK {
                return Err(st);
            }
            Ok(id)
        }

        /// The SDK's own version (for the log, not the sensor's).
        pub fn driver_version() -> (u16, u16, u16) {
            let (mut major, mut minor, mut patch) = (0u16, 0u16, 0u16);
            let mut hash = [0u8; 12];
            let mut ahead = 0i32;
            unsafe {
                bmv080_get_driver_version(
                    &mut major,
                    &mut minor,
                    &mut patch,
                    hash.as_mut_ptr(),
                    &mut ahead,
                );
            }
            (major, minor, patch)
        }

        /// `bmv080_set_parameter` with the two duty-cycling knobs:
        /// `integration_time` (seconds, f32) is the measurement window;
        /// `duty_cycling_period` (seconds, u16) is window + sleep, and
        /// must exceed `integration_time` by at least 2 s.
        pub fn configure_duty_cycle(
            &mut self,
            integration_time_s: f32,
            period_s: u16,
        ) -> Result<(), Status> {
            let st = unsafe {
                bmv080_set_parameter(
                    self.handle,
                    c"integration_time".as_ptr(),
                    &integration_time_s as *const f32 as *const c_void,
                )
            };
            if st != OK {
                return Err(st);
            }
            let st = unsafe {
                bmv080_set_parameter(
                    self.handle,
                    c"duty_cycling_period".as_ptr(),
                    &period_s as *const u16 as *const c_void,
                )
            };
            if st != OK {
                return Err(st);
            }
            Ok(())
        }

        /// Start duty-cycling measurement. `tick` supplies milliseconds
        /// to the SDK's scheduler.
        pub fn start_duty_cycling(&mut self) -> Result<(), Status> {
            let st = unsafe {
                bmv080_start_duty_cycling_measurement(self.handle, tick_ms, DUTY_CYCLING_MODE_0)
            };
            if st != OK {
                return Err(st);
            }
            Ok(())
        }

        /// Pump the SDK once; when a measurement period finished,
        /// `sink` is overwritten with its output.
        ///
        /// The SDK wants this at least once per second in duty-cycling
        /// mode; it tolerates far more frequent calls. Cheap enough to
        /// call every few hundred milliseconds in the main loop.
        pub fn serve(&mut self, sink: &mut Measurement) -> Result<(), Status> {
            let st = unsafe {
                bmv080_serve_interrupt(
                    self.handle,
                    data_ready,
                    sink as *mut Measurement as *mut c_void,
                )
            };
            if st != OK {
                return Err(st);
            }
            Ok(())
        }

        /// `bmv080_stop_measurement`.
        pub fn stop(&mut self) -> Result<(), Status> {
            let st = unsafe { bmv080_stop_measurement(self.handle) };
            if st != OK {
                return Err(st);
            }
            Ok(())
        }
    }

    impl Drop for Sensor<'_> {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    bmv080_close(&mut self.handle);
                }
            }
        }
    }

    // ---- the callbacks the blob calls --------------------------------
    //
    // All three run inside an SDK call, on this thread, with the Sensor
    // holding a live `&mut I2c` — the `sercom` pointer is that borrow's
    // address, valid for the duration of the call and no longer. None of
    // them may unwind: an unwind over an extern "C" boundary is UB.

    /// Read `payload_length` big-endian u16 words from `header`.
    ///
    /// Wire sequence (combridge.c): write `[hdr<<1 >>8, hdr<<1 &0xFF]`,
    /// STOP, then read `len*2` bytes and byteswap each word to LE.
    extern "C" fn i2c_read(
        sercom: SercomHandle,
        header: u16,
        payload: *mut u16,
        payload_length: u16,
    ) -> i8 {
        let i2c = unsafe { &mut *(sercom as *mut I2c<Blocking>) };
        let hdr = header << 1;
        let hdr_bytes = [(hdr >> 8) as u8, (hdr & 0xFF) as u8];
        if i2c.write(I2C_ADDRESS, &hdr_bytes).is_err() {
            return -1;
        }
        let byte_len = payload_length as usize * 2;
        // Read straight into the blob's buffer; the words are BE on the
        // wire and swapped in place afterwards.
        let buf = unsafe { core::slice::from_raw_parts_mut(payload as *mut u8, byte_len) };
        if i2c.read(I2C_ADDRESS, buf).is_err() {
            return -1;
        }
        let words = unsafe { core::slice::from_raw_parts_mut(payload, payload_length as usize) };
        for w in words.iter_mut() {
            *w = w.swap_bytes();
        }
        0
    }

    /// Write `payload_length` big-endian u16 words to `header`.
    ///
    /// Wire sequence (combridge.c): the address byte gains the header's
    /// MSB (`(addr<<1)|(header>>15)`) — the blob puts the R/W flag there,
    /// and for a *write* call it is always 0. If that ever changes the
    /// transfer cannot be expressed as a plain write and we refuse.
    extern "C" fn i2c_write(
        sercom: SercomHandle,
        header: u16,
        payload: *const u16,
        payload_length: u16,
    ) -> i8 {
        if header >> 15 != 0 {
            return -1;
        }
        let i2c = unsafe { &mut *(sercom as *mut I2c<Blocking>) };
        let hdr = header << 1;
        let mut buf = Vec::with_capacity(2 + payload_length as usize * 2);
        buf.push((hdr >> 8) as u8);
        buf.push((hdr & 0xFF) as u8);
        for i in 0..payload_length as usize {
            let w = unsafe { *payload.add(i) };
            buf.push((w >> 8) as u8);
            buf.push((w & 0xFF) as u8);
        }
        if i2c.write(I2C_ADDRESS, &buf).is_err() {
            return -1;
        }
        0
    }

    extern "C" fn delay_ms(ms: u32) -> i8 {
        Delay::new().delay_millis(ms);
        0
    }

    extern "C" fn tick_ms() -> u32 {
        crate::uptime_ms() as u32
    }

    extern "C" fn data_ready(out: Bmv080Output, params: *mut c_void) {
        let sink = unsafe { &mut *(params as *mut Measurement) };
        sink.pm1_ug_m3 = out.pm1_mass_concentration;
        sink.pm2_5_ug_m3 = out.pm2_5_mass_concentration;
        sink.pm10_ug_m3 = out.pm10_mass_concentration;
        sink.obstructed = out.is_obstructed;
        sink.outside_range = out.is_outside_measurement_range;
        sink.seq = sink.seq.wrapping_add(1);
    }

    // ---- newlib glue the blob drags in --------------------------------
    //
    // lib_bmv080.a calls `malloc`/`snprintf`/`pow`, so linking it pulls
    // pieces of newlib that expect a C runtime we do not have: Espressif's
    // newlib routes its reentrancy state through `__getreent()` (normally
    // supplied by ESP-IDF's newlib component), and `malloc` grows through
    // `_sbrk`. These are the two symbols the linker cannot resolve; both
    // are supplied here, and only exist in a `bmv080-sdk` build.

    // newlib's global reentrancy struct — provided by libc.a itself.
    unsafe extern "C" {
        static _impure_ptr: *mut c_void;
    }

    /// `__getreent` — ESP-IDF normally defines this to hand newlib the
    /// per-task reentrancy struct; on this firmware there is exactly one
    /// thread of C execution, so the global `_impure_ptr` is it.
    #[unsafe(no_mangle)]
    extern "C" fn __getreent() -> *mut c_void {
        unsafe { _impure_ptr }
    }

    /// Bytes reserved for the blob's `malloc` calls.
    ///
    /// Bump-only: `_sbrk` cannot shrink, so `free` hands bytes to a
    /// newlib free list that is never returned. Sound here because the
    /// SDK's allocation pattern is open-time (it allocates its working
    /// buffers when the handle is created and holds them until close) —
    /// serve calls do not allocate per reading.
    const C_HEAP_SIZE: usize = 64 * 1024;

    /// `_sbrk` — newlib's heap edge. Serves out of a reserved static
    /// arena; returns -1 on exhaustion, which is the contract malloc
    /// checks.
    #[unsafe(no_mangle)]
    extern "C" fn _sbrk(incr: isize) -> *mut c_void {
        use core::sync::atomic::{AtomicUsize, Ordering};
        static mut C_HEAP: [u8; C_HEAP_SIZE] = [0; C_HEAP_SIZE];
        static CURSOR: AtomicUsize = AtomicUsize::new(0);
        let start = CURSOR.fetch_add(incr.max(0) as usize, Ordering::Relaxed);
        let end = start + incr.max(0) as usize;
        if end > C_HEAP_SIZE {
            CURSOR.store(start, Ordering::Relaxed);
            return usize::MAX as *mut c_void; // (void*)-1
        }
        unsafe { core::ptr::addr_of_mut!(C_HEAP).cast::<u8>().add(start) as *mut c_void }
    }
}
