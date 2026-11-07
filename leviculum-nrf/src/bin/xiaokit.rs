//! Firmware entry point for the Seeed XIAO nRF52840 + Wio-SX1262 kit
//!
//! Runs a Reticulum transport node with three interfaces:
//! - Interface 0: USB CDC-ACM serial (HDLC framing) to host
//! - Interface 1: SX1262 LoRa radio, on the Wio-SX1262 module
//! - Interface 2: BLE peripheral (Columba v2.2 protocol)
//!
//! The transport engine routes packets between all interfaces.
//!
//! Two Seeed modules on the XIAO's headers and no carrier board: this
//! file is the solar node's binary minus the carrier — the radio map is
//! identical (the Wio-SX1262 sits on the same pads in both products), and
//! what actually changes is everything the carrier used to provide. The
//! LED is the XIAO module's own common-anode RGB, **active low** where
//! the carrier's were active high; there is no QSPI part on the plain
//! XIAO and no user button; and the L76K GNSS, an add-on here, has no
//! enable line of its own — a kit without one reports `no-hardware` at
//! run time (`boards/xiaokit.rs`, `docs/src/firmware/boards.md` Family C).

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output};
use embassy_nrf::saadc::Input;
use embassy_nrf::{spim, Peri, Peripherals};

use leviculum_nrf::boards::xiaokit;
use leviculum_nrf::common_peripherals;
use leviculum_nrf::node::board::{Board, LoraWiring, Wiring};

/// The Seeed XIAO nRF52840 + Wio-SX1262 kit: its pin map and the few
/// behaviours that are its own. Everything else is `leviculum_nrf::node`.
struct XiaoKit {
    high_charge_pin: Option<Peri<'static, xiaokit::HighCharge>>,
    led_pin: Option<Peri<'static, xiaokit::LedPin>>,
    /// Held for the life of `main`: dropping the output would release the
    /// BQ25101's ISET line.
    high_charge: Option<Output<'static>>,
    led: Option<Output<'static>>,
    /// The blue cell of the RGB, handed to the Blink app when it is built in.
    #[cfg(feature = "lxmf-blink")]
    blue_pin: Option<Peri<'static, xiaokit::LedNotificationPin>>,
    /// The six QSPI pads of the XIAO module.
    qspi: Option<QspiPins>,
}

/// The XIAO module's QSPI nets (`boards/solarnode.rs` has the schematic
/// citation): `P0.21` SCK, `P0.25` CSN, `P0.20`..`P0.24` the four IO lines.
struct QspiPins {
    qspi: Peri<'static, embassy_nrf::peripherals::QSPI>,
    sck: Peri<'static, embassy_nrf::peripherals::P0_21>,
    csn: Peri<'static, embassy_nrf::peripherals::P0_25>,
    io0: Peri<'static, embassy_nrf::peripherals::P0_20>,
    io1: Peri<'static, embassy_nrf::peripherals::P0_24>,
    io2: Peri<'static, embassy_nrf::peripherals::P0_22>,
    io3: Peri<'static, embassy_nrf::peripherals::P0_23>,
}

impl Board for XiaoKit {
    const NAME: &'static str = "XiaoKit";
    const CONFIG: &'static leviculum_nrf::boards::BoardConfig = &xiaokit::CONFIG;
    const PANIC_LED: (u8, u8, bool) = (
        xiaokit::PANIC_LED_PORT,
        xiaokit::PANIC_LED_PIN,
        xiaokit::PANIC_LED_ACTIVE_LOW,
    );
    // A distinct LED for HardFault — the blue cell of the RGB on P0.06,
    // active LOW like the green — so a bring-up can tell a fault from a
    // panic across the room, before the first serial line exists. Same
    // reasoning as the Pocket V2's (`bin/rak4631.rs`).
    const HARDFAULT_LED: (u8, u8, bool) = (0, 6, true);
    const GNSS_SPAWNED_LINE: &'static str = "gnss task spawned (L76K)";

    fn wire(p: Peripherals) -> Wiring<Self> {
        let common = common_peripherals!(p);
        Wiring {
            common,
            // LoRa on SPIM2, the instance the shared `lora::init` signature
            // dictates. Pin map from `boards/xiaokit.rs`; note that SCK/MISO/MOSI
            // are the XIAO's P1.13/P1.14/P1.15 pads while CS and the three control
            // lines are on port 0.
            lora: LoraWiring {
                sck: p.P1_13.into(),
                mosi: p.P1_15.into(),
                miso: p.P1_14.into(),
                cs: p.P0_04.into(),    // NSS
                reset: p.P0_28.into(), // RESET
                busy: p.P0_29.into(),  // BUSY
                dio1: p.P0_03.into(),  // DIO1
        // The line the other two boards do not have: DIO2 steers the
        // transmit side of the Wio-SX1262's switch, the host steers the
        // receive side. The driver asserts it for a listening window and
        // releases it before every key-up (`sx1262.rs`); nothing above the
        // interface knows it exists.
                rx_enable: Some(p.P0_05.into()), // RXEN
                spi_freq: spim::Frequency::M4,
            },
    // XIAO L76K on UARTE0 — an add-on module here, not part of the kit,
    // so `no-hardware` is a perfectly ordinary answer. Sweep starts at
    // the module's 9600 default. Pin naming is from the MCU's side, as
    // everywhere else in this tree: the MCU transmits on P1.11 and
    // receives on P1.12, the direction the kit variant states in words
    // (`GPS_TX_PIN D6 // This is data from the MCU`); `boards/xiaokit.rs`
    // carries the citation and a pointer to the solar node's longer
    // argument. A crossed pair is silent, not an error — it remains the
    // cheapest first experiment if a powered module reports
    // `no-hardware`.
    //
    // P0.02 is the standby/wakeup control the driver holds high (the
    // variant sets no GPS_STANDBY_ACTIVE, so the upstream default "LOW is
    // standby" applies and HIGH is awake). There is no PPS line on this
    // board either, hence `pps: None` — P0.31, which would carry it on a
    // XIAO, is the battery ADC — so this is an NMEA-only time source
    // (#166).
    //
    // And there is no power enable at all: the variant defines no
    // `GPS_EN` for the kit (that line is the solar node's carrier's), so
    // `power_enable` is `None` and a fitted module is simply powered.
    // Its reset is likewise not broken out — the pair of lines that
    // board file lists as "declared, not driven" do not exist here to
    // declare.
            #[cfg(feature = "gnss")]
            gnss:             leviculum_nrf::gnss::GnssWiring {
                uarte: p.UARTE0,
                timer: p.TIMER1,
                ppi_a: p.PPI_CH0,
                ppi_b: p.PPI_CH1,
                rx: p.P1_12.into(),
                tx: p.P1_11.into(),
                pps: None,
                standby: Some(p.P0_02.into()),
                // No receiver supply switch on this board: a fitted L76K
                // is powered by being plugged in.
                power_enable: None,
                module: leviculum_nrf::gnss::ModuleKind::QuectelL76k,
            },
    // Battery sense on P0.31 (AIN7) through the XIAO module's own 1 M/510 k
    // divider, with P0.14 as its enable — **active low** here, unlike the
    // T114's, which is why the polarity travels with the pin
    // (`boards/xiaokit.rs`, `battery::DividerEnable`). Asserted only for
    // the duration of a sample so 1.51 MΩ does not sit across the pack for
    // a winter.
    //
    // `for_divider` and not `for_board`: this board states its divider as
    // the two resistors rather than as their ratio, and both halves of the
    // channel configuration come out of them — the 2.9608 the conversion
    // divides by AND the 20 µs acquisition window 338 kΩ of source needs,
    // where the other two boards' dividers are inside the 10 µs default.
            #[cfg(feature = "battery")]
            battery: leviculum_nrf::node::board::BatteryWiring {
                saadc: p.SAADC,
                pin: p.P0_31.degrade_saadc(),
                divider_enable: Some(leviculum_nrf::battery::DividerEnable {
                    pin: p.P0_14.into(),
                    active: xiaokit::ADC_CTRL_ACTIVE,
                }),
                scale: leviculum_nrf::battery::BatteryScale::for_divider(xiaokit::BATTERY_DIVIDER),
            },
            board: XiaoKit {
                high_charge_pin: Some(p.P0_13),
                led_pin: Some(p.P0_30),
                high_charge: None,
                led: None,
                #[cfg(feature = "lxmf-blink")]
                blue_pin: Some(p.P0_06),
                qspi: Some(QspiPins {
                    qspi: p.QSPI,
                    sck: p.P0_21,
                    csn: p.P0_25,
                    io0: p.P0_20,
                    io1: p.P0_24,
                    io2: p.P0_22,
                    io3: p.P0_23,
                }),
            },
        }
    }

    fn power_up(&mut self) {
    // No VEXT rail here: two stacked modules have no switched peripheral
    // supply of the T114's kind at all. The one rail-shaped pin the kit
    // does own is the BQ25101's ISET on P0.13 — driven low now for the
    // 100 mA charge current upstream's `initVariant` chooses
    // (`boards/xiaokit.rs`), and held for the life of `main`.
        self.high_charge = Some(xiaokit::high_charge(self.high_charge_pin.take().unwrap()));
        self.led = Some(xiaokit::led(self.led_pin.take().unwrap()));
    }

    async fn qspi(&mut self) {
    // The XIAO's 2 MB P25Q16H (`boards/xiaokit.rs`, QSPI block): ask it its
    // name once — JEDEC id read, then a look for a record log. Nothing is
    // erased, written or formatted and no status register is touched, so
    // after this boot the part holds what it held before. The `[QSPI]
    // JEDEC` line says `state=ok`, `state=unexpected-part`, or
    // `state=no-answer` for a unit whose footprint is empty.
    if let (Some(q), Some(part)) = (self.qspi.take(), xiaokit::CONFIG.qspi_part) {
        leviculum_nrf::qspi::identify_and_mount_bounded(
            q.qspi,
            q.sck.into(),
            q.csn.into(),
            q.io0.into(),
            q.io1.into(),
            q.io2.into(),
            q.io3.into(),
            part,
        )
        .await;
    } else {
        // Unreachable while the board declares a part and keeps its pins;
        // kept so a capture never shows no `[QSPI]` line at all.
        leviculum_nrf::log::log_fmt_critical(
            "[QSPI] ",
            format_args!("NONE board=xiaokit reason=no-part-declared-see-boards-xiaokit-rs"),
        );
    }
    }

    fn boot_indicator(&mut self, _spawner: &Spawner) {
        let led = self.led.as_mut().unwrap();
    // Boot blink. Inverted even against the solar node's: this board's
    // green LED is the common-anode RGB cell, **active low**
    // (`boards/xiaokit.rs`), so Low is the lit level and the pin ends
    // where `xiaokit::led` left it, dark.
        led.set_level(Level::Low);
        for _ in 0..12_000_000u32 {
            cortex_m::asm::nop();
        }
        led.set_level(Level::High);
    }

    // The Blink app's LED: the blue cell on P0.06, active LOW like the
    // green one. The HardFault handler drives the same pin straight from
    // the registers, so a fault overrides the blink — intended.
    #[cfg(feature = "lxmf-blink")]
    fn blink_led(&mut self) -> Option<Output<'static>> {
        self.blue_pin.take().map(xiaokit::led_notification)
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    leviculum_nrf::node::run::<XiaoKit>(spawner).await
}
