//! Boot: the debug log, the status LED and the five-second banner.

use esp_hal::delay::Delay;
use esp_hal::gpio::{AnyPin, Level, Output, OutputConfig};
use esp_hal::peripherals::USB_DEVICE;

use crate::node::board::Board;
use crate::UsbLog;

/// How often the board re-states what it is.
///
/// Five seconds, the same cadence the nRF boards use, for the same reason:
/// a reader that attaches after the boot window — which is every reader,
/// since the port only enumerates once the firmware is running — must not
/// have to reset the board to learn what is on it.
pub const BANNER_INTERVAL_MS: u32 = 5_000;

/// What boot leaves behind for the rest of the run.
pub struct Booted {
    pub log: UsbLog<'static>,
    pub led: Output<'static>,
    pub led_active_high: bool,
}

/// First thing after `init`, and over the port the flasher reads back:
/// open the debug log, say which image and board this is, and park the
/// status LED dark (the only boot proof on a battery with no cable).
pub fn early<B: Board>(usb: USB_DEVICE<'static>, led_pin: AnyPin<'static>) -> Booted {
    let mut log = UsbLog::new(usb);
    log.identify(B::CONFIG);
    let led = Output::new(
        led_pin,
        if B::LED_ACTIVE_HIGH {
            Level::Low
        } else {
            Level::High
        },
        OutputConfig::default(),
    );
    Booted {
        log,
        led,
        led_active_high: B::LED_ACTIVE_HIGH,
    }
}

/// One short flash of the status LED, in whichever direction the board
/// wires it.
pub fn blink(led: &mut Output<'_>, active_high: bool) {
    let delay = Delay::new();
    if active_high {
        led.set_high();
        delay.delay_millis(30);
        led.set_low();
    } else {
        led.set_low();
        delay.delay_millis(30);
        led.set_high();
    }
}

/// A peripheral that cannot be configured: say so and keep the banner
/// running forever. A board that boots and reports a broken bus is
/// diagnosable; one that halts is not.
pub fn halt_blinking<B: Board>(boot: &mut Booted) -> ! {
    loop {
        blink(&mut boot.led, boot.led_active_high);
        Delay::new().delay_millis(BANNER_INTERVAL_MS);
        boot.log.identify(B::CONFIG);
    }
}
