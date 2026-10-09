//! Reticulum firmware for ESP32-class boards.
//!
//! The sibling of `leviculum-nrf`, on the other half of the hardware we
//! own: the Heltec V3/V4 and T-Beam boards, and the XIAO ESP32-S3 +
//! Wio-SX1262 kit, that today run somebody else's firmware because we had
//! none of our own for the family. The first target is the Heltec WiFi
//! LoRa 32 V4 (ESP32-S3R2 + SX1262 + KCT8103L front end); the second is
//! the XIAO kit (ESP32-S3R8 + the same bare SX1262 the nRF kit mounts,
//! on different pads).
//!
//! # What this crate is, at this step
//!
//! It boots, it says what it is, and it holds the SX1262's SPI port open.
//! Nothing goes on the air. The point of the step is not the banner — it
//! is that the shape a second and third board plug into exists before the
//! first board's details have hardened around it:
//!
//!  * the per-board facts live in [`boards`], one module per wiring, each
//!    constant carrying the document it was read out of;
//!  * the per-class facts live here: the init order, the clock, the USB
//!    serial the banner leaves by, the banner itself;
//!  * the radio is reached through [`leviculum_core::sx126x`]'s two
//!    traits, implemented once in [`sx1262`], so the sequences that decide
//!    what goes on the air stay in the crate that has host tests.
//!
//! # The build stamp
//!
//! [`FW_BUILD_STAMP`] is the same one contiguous literal `leviculum-nrf`
//! carries, produced by the same `build.rs`, printed after the same
//! `[FW_BUILD] ` prefix. The rig's flasher and the reviewer's boot proof
//! grep one shape; a firmware family that printed a second shape would be
//! a firmware family the proof cannot read.

#![no_std]

pub mod automation;
pub mod bme690;
pub mod bmv080;
pub mod boards;
pub mod ds18b20;
pub mod pid;
pub mod pid_lxmf;
pub mod radio;
pub mod store;
pub mod sx1262;

use core::mem::MaybeUninit;

use embedded_alloc::LlffHeap as Heap;
#[cfg(any(feature = "bmv080-sdk", feature = "bme690"))]
use esp_hal::i2c::master::I2c;
use esp_hal::{
    clock::CpuClock,
    peripherals::{Peripherals, USB_DEVICE},
    time::{Duration, Instant},
    usb::usb_serial_jtag::UsbSerialJtag,
    Blocking,
};

use boards::BoardConfig;

/// The build identity this image carries, as ONE contiguous string
/// literal.
///
/// It is printed verbatim after `[FW_BUILD] ` by the boot line and by the
/// periodic banner. Being a single literal is the point, and the reason is
/// the one `leviculum-nrf/src/lib.rs` states: the flash runner greps the
/// same text out of the flat image it is about to write, so what it
/// expects to read back is taken from the image rather than from the
/// working tree. Two `env!` arguments formatted at the call site would put
/// the sha in its own rodata entry with nothing next to it to grep for.
///
/// `dirty` is part of the stamp, not decoration: two images built from the
/// same commit with different working trees are different images, and a
/// rig that confirmed only the sha would call them the same one.
pub const FW_BUILD_STAMP: &str = concat!(
    "git_sha=",
    env!("LEVICULUM_GIT_SHA"),
    " dirty=",
    env!("LEVICULUM_GIT_DIRTY")
);

// ---------------------------------------------------------------------
// Heap
// ---------------------------------------------------------------------

#[global_allocator]
static HEAP: Heap = Heap::empty();

/// Bytes reserved for the heap.
///
/// Nothing in this crate allocates yet. The allocator is here because
/// `leviculum-core` is `no_std + alloc`, and a binary that links `alloc`
/// without a `#[global_allocator]` does not link at all — so the choice is
/// not whether to have a heap but how big to say it is before anything has
/// measured it. 128 KiB is sized for the real consumer that arrived with
/// the transport: `NodeCore` is boxed onto the heap (a >40 KiB struct
/// the `build_boxed` path exists to keep off the stack), and the LXMF
/// message paths allocate per packet on top of it. Still well under the
/// ESP32-S3's 512 KiB SRAM, and the watermark that would raise it is
/// runtime-measurable.
pub const HEAP_SIZE: usize = 128 * 1024;

/// Initialise the heap allocator. Called once by [`init`].
fn init_heap() {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: called once at startup, before anything concurrent exists.
    // `addr_of!` avoids forming a reference to the `static mut`.
    unsafe {
        let heap_start = core::ptr::addr_of!(HEAP_MEM) as usize;
        HEAP.init(heap_start, HEAP_SIZE);
    }
}

// ---------------------------------------------------------------------
// Class init
// ---------------------------------------------------------------------

/// Bring up the SoC and the heap, and hand back the peripherals.
///
/// The order is the class's, not the board's, and it is the whole reason
/// this function exists rather than three lines at the top of each binary:
///
/// 1. `esp_hal::init` — clocks and the watchdogs, before anything reads a
///    clock or a timer;
/// 2. the heap, before any code that might allocate;
/// 3. return, leaving the peripherals to the board's binary to hand out.
///
/// The CPU clock is taken at maximum. Nothing here is power-tuned yet and
/// pretending otherwise with a lower number would be a decision without a
/// measurement behind it.
pub fn init() -> Peripherals {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    init_heap();
    peripherals
}

/// Milliseconds since boot, the stamp every log line carries.
pub fn uptime_ms() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

// ---------------------------------------------------------------------
// The shared sensor bus
// ---------------------------------------------------------------------

/// A shareable handle to the sensor I2C bus.
///
/// Only exists when a sensor-driver feature is on — a probe-only build
/// never shares the bus.
///
/// The add-on sensors this crate drives (BMV080, BME690, and whatever
/// else lands on the Qwiic pair) can share the one `I2c` peripheral, and
/// each is reached through C callbacks that carry a `void*` — a shared
/// `&mut` cannot model that: two drivers would each need an outstanding
/// exclusive borrow.
///
/// So the handle is a raw pointer, and the safety argument is
/// topological: every driver call is synchronous on the single firmware
/// thread, the FFI callback materialises a `&mut` only for the duration
/// of one transfer, and no two `&mut`s to the bus are ever live at once.
/// `SharedI2c` exists to keep that contract visible — it is `Copy`, so
/// any number of drivers may hold it; exclusivity is temporal, not
/// lexical.
#[cfg(any(feature = "bmv080-sdk", feature = "bme690"))]
#[derive(Clone, Copy)]
pub struct SharedI2c<'d> {
    ptr: *mut I2c<'d, Blocking>,
}

#[cfg(any(feature = "bmv080-sdk", feature = "bme690"))]
impl<'d> SharedI2c<'d> {
    /// Capture the bus. The caller hands over `&mut` once; afterwards the
    /// bus may only be driven through `SharedI2c`s — code that keeps its
    /// own `&mut` to the same `I2c` while drivers hold a `SharedI2c` has
    /// broken the contract.
    pub fn new(i2c: &mut I2c<'d, Blocking>) -> Self {
        Self { ptr: i2c }
    }

    /// The raw pointer the C callback boundary threads back as `void*`.
    ///
    /// The pointer names the bus itself — a callback receives it and
    /// reconstructs `&mut` for its transfer.
    pub(crate) fn as_sercom(&self) -> *mut I2c<'d, Blocking> {
        self.ptr
    }
}

// ---------------------------------------------------------------------
// The debug log
// ---------------------------------------------------------------------

/// How long a single byte may wait for room in the USB FIFO.
///
/// esp-hal's own `UsbSerialJtag::write` spins on the endpoint status until
/// the host drains it. With no host attached that spin does not end, and a
/// board whose first act after init is to print would hang on the bench
/// the moment it is powered from a battery instead of a cable. So the
/// writer here is the non-blocking one with a deadline, and a line that
/// cannot be placed is dropped: diagnostics may never be the reason the
/// firmware stops.
const USB_BYTE_TIMEOUT: Duration = Duration::from_millis(2);

/// Upper bound on one formatted log line.
///
/// `leviculum_log_line::format_line` truncates the body rather than the
/// stamp, so this is a limit on what is readable, never on what is
/// well-formed.
const LINE_MAX: usize = 160;

/// The board's debug log: `<prefix><body> t=<uptime_ms>\r\n` out of the
/// SoC's USB Serial/JTAG port.
///
/// There is no USB-to-UART bridge on these boards — the Type-C connector
/// goes straight to the SoC's own USB pins — so this peripheral IS the
/// debug port, and what a capture on the host sees is what is written
/// here.
pub struct UsbLog<'d> {
    port: UsbSerialJtag<'d, Blocking>,
}

impl<'d> UsbLog<'d> {
    /// Open the USB Serial/JTAG port.
    pub fn new(usb_device: USB_DEVICE<'d>) -> Self {
        Self {
            port: UsbSerialJtag::new(usb_device),
        }
    }

    /// Emit one line, shaped by [`leviculum_log_line::format_line`].
    ///
    /// The shape is not this crate's to choose. `lnflash::verify` splits
    /// host-side captures on `[FW_BUILD]`, periculum's
    /// `fw_build_version_in` greps for it, and both were written against
    /// the nRF boards. Going through the same crate that pins the shape
    /// for those boards is what keeps one reader able to read both
    /// families.
    pub fn line(&mut self, prefix: &str, args: core::fmt::Arguments) {
        let mut buf = [0u8; LINE_MAX];
        let line = leviculum_log_line::format_line(&mut buf, prefix, args, uptime_ms());
        for byte in line {
            if !self.put(*byte) {
                // No reader, or a reader that stopped draining. Abandon
                // the rest of the line rather than the boot.
                return;
            }
        }
        let _ = self.port.flush_tx_nb();
    }

    /// Drain whatever the host has sent into `buf` — command ingress on
    /// the debug port (the `PID` lines `xiao_s3` parses). Returns the
    /// byte count; 0 means nothing pending. Byte-at-a-time, nonblocking:
    /// `read_byte` reports `WouldBlock` when the FIFO is empty.
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() {
            match self.port.read_byte() {
                Ok(b) => {
                    buf[n] = b;
                    n += 1;
                }
                Err(_) => break,
            }
        }
        n
    }

    /// State what this image is and what board it thinks it is on.
    ///
    /// Two lines, in this order, because they answer different questions
    /// and a capture may only catch one of them: `[FW_BUILD]` says which
    /// commit produced the bytes, `[BOARD]` says which wiring those bytes
    /// were compiled for. A board flashed with the right commit and the
    /// wrong board file is a real failure mode and the first line alone
    /// cannot show it.
    pub fn identify(&mut self, config: &'static BoardConfig) {
        self.line("[FW_BUILD] ", format_args!("{FW_BUILD_STAMP}"));
        self.line(
            "[BOARD] ",
            format_args!(
                "id={} name={} gnss={} heap={}",
                config.log_prefix,
                config.board_name,
                if config.gnss_onboard {
                    "onboard"
                } else {
                    "none"
                },
                HEAP_SIZE
            ),
        );
    }

    /// Place one byte, or report that the FIFO never freed up.
    fn put(&mut self, byte: u8) -> bool {
        let deadline = Instant::now() + USB_BYTE_TIMEOUT;
        loop {
            if self.port.write_byte_nb(byte).is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Panic
// ---------------------------------------------------------------------

/// Halt on panic.
///
/// Deliberately the whole of it at this step. The nRF crate's handler
/// writes the message into `.uninit` RAM, counts the panic across resets
/// and replays it on the next boot; every one of those depends on machinery
/// this crate does not have yet (a noinit section, a persistent counter, a
/// boot-time replay). A handler that pretended to do any of it would be a
/// handler nobody could trust, so this one does the one thing it can do
/// honestly: stop, and leave the board visibly dead rather than quietly
/// wrong.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
