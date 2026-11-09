//! The ESP LNode: one firmware, many boards.
//!
//! A board binary is a hardware definition (a [`Board`] implementation)
//! plus a call to [`run`]. Everything an LNode does that does not depend on
//! which unit it runs on is split by function into the modules below, in
//! the order the boot reaches them. It mirrors `leviculum-nrf/src/node`,
//! and like it runs on the Embassy executor (here `esp-rtos`' thread-mode
//! executor over `esp-hal`): the main loop is an async function and an app
//! spawns Embassy tasks for anything that must run on its own.
//!
//! | module | what it owns |
//! |---|---|
//! | [`board`] | the seam: pin map, LoRa profile, LED polarity |
//! | [`boot`] | the debug log, the status LED, the banner |
//! | [`carriers`] | the LoRa radio: bring-up, receive, transmit |
//! | [`peripherals`] | the add-on I2C sensors (BMV080, BME690) |
//! | [`reticulum`] | identity and the transport node |
//! | [`storage`] | the shared internal-flash handle |
//! | [`app`] | the app seam — [`App`] and [`Ctx`] — and the applications: `pid_app` (default), `echo` (`lxmf-echo`), `blink` (`lxmf-blink`) |
//! | [`mainloop`] | the cooperative event loop |
//! | `responder` | the LXMF inbox shared by the echo and blink apps |
//! | `wifi` | the WiFi carrier: UDP and TCP interfaces 1 and 2 (`wifi` feature) |

pub mod app;
pub mod board;
pub mod boot;
pub mod carriers;
#[cfg(feature = "display")]
pub mod display;
pub mod mainloop;
pub mod peripherals;
#[cfg(any(feature = "lxmf-echo", feature = "lxmf-blink"))]
pub mod responder;
pub mod reticulum;
pub mod storage;
#[cfg(feature = "wifi")]
pub mod wifi;

#[cfg(all(feature = "lxmf-echo", feature = "lxmf-blink"))]
compile_error!("`lxmf-echo` and `lxmf-blink` are alternative apps: enable one");

pub use app::{App, Ctx};
pub use board::{Board, Wiring};

use embassy_executor::Spawner;
use esp_hal::rng::TrngSource;
use esp_hal::timer::timg::TimerGroup;

/// The app a build runs: the PID regulator, the echo inbox with
/// `--features lxmf-echo`, or the LED blinker with `--features lxmf-blink`.
#[cfg(feature = "lxmf-blink")]
pub type DefaultApp = app::blink::BlinkApp;
#[cfg(feature = "lxmf-echo")]
pub type DefaultApp = app::echo::EchoApp;
#[cfg(not(any(feature = "lxmf-echo", feature = "lxmf-blink")))]
pub type DefaultApp = app::pid_app::PidApp;

/// Boot an LNode on board `B` and run it forever.
pub async fn run<B: Board>(spawner: Spawner) -> ! {
    let p = crate::init();
    let Wiring {
        system,
        led,
        lora,
        sensors,
        display: display_wiring,
        mut app,
    } = B::wire(p);

    // The scheduler, its tick source and the Embassy time driver.
    let timg0 = TimerGroup::new(system.timg0);
    esp_rtos::start(timg0.timer0, system.sw_int0);

    let mut boot = boot::early::<B>(system.usb, led);
    let mut carrier = carriers::start::<B>(&mut boot, lora);
    let (sensors, i2c_bus) = peripherals::start::<B>(&mut boot, sensors);

    let mut flash = esp_storage::FlashStorage::new(system.flash);
    // The TRNG source has to be held for `Trng::try_new` to succeed — it
    // unlocks only while a TrngSource exists.
    let _trng_src = TrngSource::new(system.rng, system.adc1);
    let identity = reticulum::identity(&mut boot.log, &mut flash);
    let mut node = reticulum::build(&identity);
    if carrier.is_up() {
        reticulum::register_interface(&mut node);
    }
    // The WiFi interfaces are ids 1 and 2. The settings come from the
    // flash slot when one is stored, otherwise from the compiled env
    // defaults; either way they can be changed on the debug port
    // (`WIFI …` commands) without a rebuild.
    #[cfg(feature = "wifi")]
    if wifi::start(
        spawner,
        system.wifi,
        crate::uptime_ms(),
        &mut flash,
        &mut boot.log,
    ) {
        wifi::register_interfaces(&mut node);
    }
    let flash = storage::share(flash);

    // The status display, when the board carries an OLED: built over
    // the shared sensor bus, showing the node's identity and counters
    // from the main loop's banner tick. A panel that refuses to come
    // up is logged and the node runs on without it.
    #[cfg(feature = "display")]
    let display = display_wiring
        .and_then(|dw| display::Display::new(dw, *node.identity().hash(), i2c_bus, &mut boot.log));
    #[cfg(not(feature = "display"))]
    let _ = i2c_bus;

    let radio_up = carrier.is_up();
    // The log goes into the context; the loop keeps only the LED, unless
    // the app owns it.
    let boot::Booted {
        mut log,
        led,
        led_active_high,
    } = boot;
    app.led_active_high = led_active_high;
    let status_led = if DefaultApp::OWNS_STATUS_LED {
        app.led = Some(led);
        None
    } else {
        Some(led)
    };
    let mut ctx = Ctx {
        node: &mut node,
        carrier: &mut carrier,
        log: &mut log,
        flash,
        identity: &identity,
        spawner,
    };
    let app = DefaultApp::start(&mut ctx, app);
    if radio_up {
        mainloop::announce(&mut ctx, &app);
        ctx.log.line("[LORA] ", format_args!("mesh=announced"));
    }

    #[cfg(feature = "display")]
    {
        mainloop::run::<B, DefaultApp>(ctx, app, sensors, display, status_led, led_active_high)
            .await
    }
    #[cfg(not(feature = "display"))]
    {
        mainloop::run::<B, DefaultApp>(ctx, app, sensors, status_led, led_active_high).await
    }
}
