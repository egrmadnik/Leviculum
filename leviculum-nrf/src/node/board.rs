//! The seam between the shared LNode code and one board's hardware.
//!
//! A [`Board`] is a hardware definition: which pins carry the LoRa
//! radio, the GNSS receiver and the battery divider, which rails and
//! LEDs the board owns, and the handful of behaviours that differ
//! between units (QSPI, display, boot indicator). Everything else an
//! LNode does is identical on every unit and lives in the sibling
//! modules of [`crate::node`].
//!
//! A board binary under `src/bin/` is therefore only: a type that
//! implements [`Board`] (pin map, using `crate::boards::<board>` for the
//! constants) and a one-line `main` that calls [`crate::node::run`].

use embassy_executor::Spawner;
use embassy_nrf::gpio::AnyPin;
use embassy_nrf::{peripherals, spim, Peri, Peripherals};

use crate::boards::BoardConfig;

/// Peripherals every board hands to the shared code under the same name:
/// the SoftDevice's fixed set, the USB device, the flash controller and
/// the LoRa SPI instance (`lora::init`'s signature dictates SPIM2).
pub struct Common {
    pub nvmc: Peri<'static, peripherals::NVMC>,
    pub usbd: Peri<'static, peripherals::USBD>,
    pub spi2: Peri<'static, peripherals::SPI2>,
    pub ble: BlePeripherals,
}

/// Build a [`Common`] out of the owned [`Peripherals`] `$p` by moving the
/// shared fields out of it. A macro because only a field-by-field move
/// leaves the board's own pins in `$p` usable.
#[macro_export]
macro_rules! common_peripherals {
    ($p:ident) => {
        $crate::node::board::Common {
            nvmc: $p.NVMC,
            usbd: $p.USBD,
            spi2: $p.SPI2,
            ble: $crate::node::board::BlePeripherals {
                rtc0: $p.RTC0,
                timer0: $p.TIMER0,
                temp: $p.TEMP,
                ppi_ch19: $p.PPI_CH19,
                ppi_ch30: $p.PPI_CH30,
                ppi_ch31: $p.PPI_CH31,
                ppi_ch17: $p.PPI_CH17,
                ppi_ch18: $p.PPI_CH18,
                ppi_ch20: $p.PPI_CH20,
                ppi_ch21: $p.PPI_CH21,
                ppi_ch22: $p.PPI_CH22,
                ppi_ch23: $p.PPI_CH23,
                ppi_ch24: $p.PPI_CH24,
                ppi_ch25: $p.PPI_CH25,
                ppi_ch26: $p.PPI_CH26,
                ppi_ch27: $p.PPI_CH27,
                ppi_ch28: $p.PPI_CH28,
                ppi_ch29: $p.PPI_CH29,
                rng: $p.RNG,
            },
        }
    };
}

/// The RTC, timer, temperature sensor, PPI channels and RNG that
/// [`crate::ble::init`] hands to the SoftDevice.
pub struct BlePeripherals {
    pub rtc0: Peri<'static, peripherals::RTC0>,
    pub timer0: Peri<'static, peripherals::TIMER0>,
    pub temp: Peri<'static, peripherals::TEMP>,
    pub ppi_ch19: Peri<'static, peripherals::PPI_CH19>,
    pub ppi_ch30: Peri<'static, peripherals::PPI_CH30>,
    pub ppi_ch31: Peri<'static, peripherals::PPI_CH31>,
    pub ppi_ch17: Peri<'static, peripherals::PPI_CH17>,
    pub ppi_ch18: Peri<'static, peripherals::PPI_CH18>,
    pub ppi_ch20: Peri<'static, peripherals::PPI_CH20>,
    pub ppi_ch21: Peri<'static, peripherals::PPI_CH21>,
    pub ppi_ch22: Peri<'static, peripherals::PPI_CH22>,
    pub ppi_ch23: Peri<'static, peripherals::PPI_CH23>,
    pub ppi_ch24: Peri<'static, peripherals::PPI_CH24>,
    pub ppi_ch25: Peri<'static, peripherals::PPI_CH25>,
    pub ppi_ch26: Peri<'static, peripherals::PPI_CH26>,
    pub ppi_ch27: Peri<'static, peripherals::PPI_CH27>,
    pub ppi_ch28: Peri<'static, peripherals::PPI_CH28>,
    pub ppi_ch29: Peri<'static, peripherals::PPI_CH29>,
    pub rng: Peri<'static, peripherals::RNG>,
}

/// The SX1262 pin map of one board.
pub struct LoraWiring {
    pub sck: Peri<'static, AnyPin>,
    pub mosi: Peri<'static, AnyPin>,
    pub miso: Peri<'static, AnyPin>,
    /// NSS / chip select.
    pub cs: Peri<'static, AnyPin>,
    pub reset: Peri<'static, AnyPin>,
    pub busy: Peri<'static, AnyPin>,
    pub dio1: Peri<'static, AnyPin>,
    /// The board's external RX-enable line, or `None` where DIO2 owns the
    /// whole antenna switch (see [`crate::boards`]).
    pub rx_enable: Option<Peri<'static, AnyPin>>,
    pub spi_freq: spim::Frequency,
}

/// The battery sampler's pins: the SAADC instance, the sense input, the
/// optional divider enable and the divider's scale.
#[cfg(feature = "battery")]
pub struct BatteryWiring {
    pub saadc: Peri<'static, peripherals::SAADC>,
    pub pin: embassy_nrf::saadc::AnyInput<'static>,
    pub divider_enable: Option<crate::battery::DividerEnable>,
    pub scale: crate::battery::BatteryScale,
}

/// What [`Board::wire`] returns: the board itself plus every
/// peripheral the shared code takes.
pub struct Wiring<B: Board> {
    pub board: B,
    pub common: Common,
    pub lora: LoraWiring,
    #[cfg(feature = "gnss")]
    pub gnss: crate::gnss::GnssWiring,
    #[cfg(feature = "battery")]
    pub battery: BatteryWiring,
}

/// One hardware unit.
///
/// The consts are the board's identity; [`wire`](Self::wire) and the hooks
/// are the only places a board's pin numbers appear.
#[allow(async_fn_in_trait)]
pub trait Board: Sized {
    /// Display name used in the boot banner (`leviculum <NAME> booting`).
    const NAME: &'static str;
    /// Runtime metadata consumed by USB, flash and LoRa init.
    const CONFIG: &'static BoardConfig;
    /// Panic LED: port, pin, active-low flag.
    const PANIC_LED: (u8, u8, bool);
    /// HardFault LED: port, pin, active-low flag.
    const HARDFAULT_LED: (u8, u8, bool);
    /// The `info!` line printed once the GNSS task is spawned.
    const GNSS_SPAWNED_LINE: &'static str = "gnss task spawned";
    /// Log a `[DIAG_MEM]` line every five seconds instead of leaving the
    /// stack watermark task as the only periodic memory report.
    const DIAG_MEM_LOG: bool = false;
    /// Log every action a protocol timeout produced (bring-up diagnostic).
    const LOG_TIMEOUT_ACTIONS: bool = false;

    /// Split `p` into the shared peripherals and this board's own pins.
    /// Pure pin bookkeeping: nothing here touches hardware.
    fn wire(p: Peripherals) -> Wiring<Self>;

    /// Raise the board's rails and build its LEDs. Runs after USB is up
    /// and before the identity is loaded, as it always did.
    fn power_up(&mut self);

    /// Mount or declare absent the board's QSPI flash.
    async fn qspi(&mut self);

    /// Spawn the board's display, if it has one. Runs after the flash
    /// stores, before GNSS.
    fn spawn_display(&mut self, _spawner: &Spawner, _identity_hash: [u8; 16]) {}

    /// The board's "I booted" indication: a blink or the LED tasks. Runs
    /// once all interface adapters exist and before the main loop.
    fn boot_indicator(&mut self, _spawner: &Spawner) {}

    /// The LED the Blink app drives, as an output in its dark state, or
    /// `None` on a board that has none. Taken once, at boot.
    #[cfg(feature = "lxmf-blink")]
    fn blink_led(&mut self) -> Option<embassy_nrf::gpio::Output<'static>> {
        None
    }

    /// Extra boot-banner lines that only one board prints.
    fn after_boot_banner() {}
}
