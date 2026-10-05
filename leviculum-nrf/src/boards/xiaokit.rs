//! Seeed XIAO nRF52840 + Wio-SX1262 kit pin mappings and hardware constants
//!
//! The kit is two modules plugged together and nothing more: a plain XIAO
//! nRF52840 (nRF52840, 1 MB flash, 256 KB RAM) and a Wio-SX1262 LoRa board
//! on its headers — Seeed SKU 102010710, the combination Meshtastic builds
//! as `seeed_xiao_nrf52840_kit`. There is no carrier board between them,
//! and that is the whole difference from [`super::solarnode`]: the solar node is
//! the same XIAO + Wio-SX1262 wiring plus a carrier that adds the L76K's
//! own enable line, its two carrier LEDs, its buttons and its pack. None of
//! those exist here. What the kit does have is the XIAO module's own RGB
//! LED — common-anode, **active LOW**, the polarity the carrier's LEDs
//! invert — and the module's BQ25101 charger, whose ISET pin upstream
//! drives for 100 mA.
//!
//! The LoRa map is identical to the solar node's because the module is the
//! module: the Wio-SX1262 sits on the same XIAO pads in both products, and
//! Seeed's kit variant wires it identically. The variant carries three
//! mutually exclusive `SX126X_*` pinouts in one header — the legacy DIY
//! `xiao_ble`, the 30-pin BTB module from the ESP32-S3 kit, and the
//! shipped default under `SEEED_XIAO_NRF_KIT_DEFAULT` — so the gate
//! resolves this board's defines through the default branch alone
//! (`reference-pins.toml`, `unselected`).
//!
//! References:
//!   * <https://files.seeedstudio.com/products/SenseCAP/Wio_SX1262/Wio-SX1262%20for%20XIAO%20V1.0_SCH.pdf>
//!   * <https://files.seeedstudio.com/wiki/XIAO-BLE/Seeed-Studio-XIAO-nRF52840-Sense-v1.1.pdf>
//!     — the module's own sheet: the RGB LED and its polarity, the
//!     battery divider, the BQ25101
//!   * Meshtastic `variants/nrf52840/seeed_xiao_nrf52840_kit/{variant.h,variant.cpp}`

use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::peripherals;
use embassy_nrf::Peri;

// SX1262 LoRa radio, on the Wio-SX1262 module (SPI0 pads of the XIAO).
// Same seven pins plus RXEN as `solarnode` — the upstream comment says it
// outright: "Same for both default and I2C pinouts".
/// SX1262 SPI clock (`PIN_SPI_SCK D8`)
pub type LoRaSck = peripherals::P1_13;
/// SX1262 SPI MOSI (`PIN_SPI_MOSI D10`)
pub type LoRaMosi = peripherals::P1_15;
/// SX1262 SPI MISO (`PIN_SPI_MISO D9`)
pub type LoRaMiso = peripherals::P1_14;
/// SX1262 chip-select, active low (`SX126X_CS D4`)
pub type LoRaCs = peripherals::P0_04;
/// SX1262 reset, active low (`SX126X_RESET D2`)
pub type LoRaReset = peripherals::P0_28;
/// SX1262 busy indicator, high = busy (`SX126X_BUSY D3`)
pub type LoRaBusy = peripherals::P0_29;
/// SX1262 DIO1 interrupt output (`SX126X_DIO1 D1`)
pub type LoRaDio1 = peripherals::P0_03;
/// SX1262 external RX enable (`SX126X_RXEN D5`), high while the chip is
/// listening and low before every key-up.
///
/// Same split antenna switch as the solar node — it is the Wio-SX1262
/// module's own front end, not the carrier's, so it travels with the
/// radio to every board that carries one. DIO2 owns the transmit side
/// (`SX126X_DIO2_AS_RF_SWITCH`), this pin the receive side, and
/// `SX126X_TXEN` is `RADIOLIB_NC`. The switching lives in `sx1262.rs`;
/// see [`crate::lora`] and the front-end note in [`super`].
pub type LoRaRxEnable = peripherals::P0_05;

/// SX1262 SPI frequency in Hz (4 MHz, as on the other boards).
pub const LORA_SPI_FREQ_HZ: u32 = 4_000_000;
/// SX1262 TCXO voltage supplied via DIO3 (volts).
/// `SX126X_DIO3_TCXO_VOLTAGE 1.8` in the variant, common to all three of
/// its pinout branches.
pub const LORA_TCXO_VOLTAGE: f32 = 1.8;
/// SX1262 max TX power (dBm). Same die, same +22 dBm.
pub const LORA_MAX_POWER_DBM: i8 = 22;
/// DIO2 drives the TX side of the RF switch; [`LoRaRxEnable`] drives the
/// RX side. See the front-end note in [`super`].
pub const LORA_DIO2_AS_RF_SWITCH: bool = true;

// LEDs — the XIAO module's own RGB, common-anode and therefore ACTIVE LOW
// (`LED_STATE_ON 0`, and `initVariant` `ledOff`s all three at boot). This
// is the polarity trap relative to the solar node, whose carrier LEDs are
// active HIGH: the same `Level::High` that lights a green LED there is
// the dark level here.
/// Green LED (`PIN_LED1 = LED_GREEN` = index 13 = P0.30), **active LOW**.
///
/// Green because upstream names it `PIN_LED1`, which its architecture
/// layer turns into `LED_POWER` — the heartbeat light.
pub type LedPin = peripherals::P0_30;
/// Blue LED (`PIN_LED2 = LED_BLUE` = index 12 = P0.06), **active LOW**.
/// The HardFault light, so a panic and a fault stay distinguishable
/// across the room on a board with no other indicator.
pub type LedNotificationPin = peripherals::P0_06;
/// Red LED (`PIN_LED3 = LED_RED` = index 11 = P0.26), **active LOW**.
///
/// Declared for completeness and deliberately not driven: two indicators
/// (panic green, fault blue) cover what a headless kit can show today.
pub type LedRedPin = peripherals::P0_26;

// Buttons — none. `BUTTON_PIN` is commented out of the variant for this
// build: the only candidate pad, D0, is the L76K's standby line under the
// default pinout (`PIN_GPS_STANDBY D0`), and the I2C pinout that puts a
// button on it is not the one this board builds.

// GNSS — the XIAO L76K, an add-on module this kit does not include.
/// GNSS UART TX — **MCU → L76K** (`GPS_TX_PIN D6`, index 6 = P1.11).
///
/// Direction taken from the variant's own comment, which for once is
/// unambiguous: `#define GPS_TX_PIN D6 // This is data from the MCU` and
/// `PIN_SERIAL1_TX GPS_TX_PIN` (in the Adafruit core `PIN_SERIAL1_TX` is
/// the pin the MCU drives). Same pins the solar node gives the same
/// module; `boards/solarnode.rs` carries the longer argument for the
/// direction.
pub type GnssTx = peripherals::P1_11;
/// GNSS UART RX — L76K → MCU (`GPS_RX_PIN D7`, index 7 = P1.12,
/// "This is data from the GNSS module").
pub type GnssRx = peripherals::P1_12;
/// GNSS standby / wakeup control (`PIN_GPS_STANDBY D0`, index 0 = P0.02).
///
/// **HIGH is awake**, as on the solar node: the variant sets no
/// `GPS_STANDBY_ACTIVE`, so Meshtastic's `LOW` default applies for
/// standby and the driver holds the pin high for the life of the task.
/// Present only in the default pinout — the I2C variant reassigns D0 to
/// the button, which is why upstream warns the button "conflicts with
/// the official GNSS module".
pub type GnssStandby = peripherals::P0_02;
/// GNSS UART baud rate. The L76K ships at 9600
/// (`#define GPS_BAUDRATE 9600`), which is where the presence machine's
/// sweep starts.
///
/// The kit has **no GNSS enable and no GNSS reset** — the variant defines
/// neither (`GPS_EN` and `PIN_GPS_RESET` belong to the solar node's
/// carrier), so a fitted L76K is simply powered, and
/// `GnssWiring.power_enable` is `None`. Nor is there a PPS line: this
/// board, like the solar node, is an NMEA-only time source (#166). A kit
/// with no L76K fitted reports `no-hardware` at run time — the same
/// answer the solar node gives when its receiver fails — so one feature
/// set serves both populations and `gnss` is unconditional.
pub const GNSS_BAUD: u32 = 9600;

// Grove / I²C, on the NFC pins
/// Grove SDA (`PIN_WIRE_SDA 30` = index 30 = P0.09, NFC1), under the
/// default pinout. The I2C pinout moves the bus to D6/D7 in place of the
/// GNSS pair — a different build, not this one.
///
/// Same trap as on the other boards: these two pads are the nRF52840's
/// NFC antenna pins, GPIO only while `UICR.NFCPINS` says so. The build
/// side is covered — `embassy-nrf` is pulled with `nfc-pins-as-gpio`.
pub type GroveSda = peripherals::P0_09;
/// Grove SCL (`PIN_WIRE_SCL 31` = index 31 = P0.10, NFC2). See [`GroveSda`].
pub type GroveScl = peripherals::P0_10;

// No QSPI part — `CONFIG.qspi_part` is `None`, and the board declares no
// `Qspi*` aliases. The evidence is the same shape as the T114's and the
// RAK4631's (#384), with one step of indirection more: the kit's own
// variant comments the whole `PIN_QSPI_*` block AND the
// `EXTERNAL_FLASH_DEVICES P25Q16H` line out, and the plain XIAO's
// schematic — the module this kit ships — prints U7's value as `DNP`.
// The footprint and its six nets (P0.21/25/20/24/22/23, the same six the
// solar node maps) belong to the XIAO nRF52840 *Plus*, and a kit built
// around a Plus would carry a part — but that unit's honest board
// declaration is the solar node's ask-at-boot shape, not this one's, and
// it would fail the gate the moment an upstream tree is checked because
// the defines the table would point at are commented out upstream. None
// of it matters at run time yet: nothing in this firmware uses the
// external flash for storage (`boards/solarnode.rs` says why).

// Battery
/// Battery voltage sense (`BATTERY_PIN = PIN_VBAT` = index 32 = P0.31,
/// AIN7). The divider is the XIAO module's own, identical to the solar
/// node's.
pub type BatteryAdc = peripherals::P0_31;
/// Divider enable (`ADC_CTRL = VBAT_ENABLE` = index 14 = P0.14), **active
/// LOW** — `ADC_CTRL_ENABLED LOW`, the pin sinks the low side of the
/// divider. Same net, same polarity as the solar node; the full argument
/// for switching it per sample rather than leaving a 1.5 MΩ leak across
/// the pack is in `boards/solarnode.rs`.
pub type AdcCtrl = peripherals::P0_14;
/// The level on [`AdcCtrl`] that switches the divider ON.
pub const ADC_CTRL_ACTIVE: Level = Level::Low;
/// The battery divider: R17 = 1 MΩ from the terminal to [`BatteryAdc`],
/// R18 = 510 kΩ from the pin down to [`AdcCtrl`].
///
/// Stated for this module in `seeed_xiao_nrf52840_kit/variant.h` beside
/// its `ADC_MULTIPLIER` comment (`R17=1M, R18=510k`) — the kit header is
/// the *primary* source for these resistors here, where the solar node
/// quotes it as the sibling. Factor (1000 + 510) / 510 = 2.9608; the
/// 338 kΩ source resistance and the 20 µs acquisition window it forces
/// are the same on this module, and `BatteryScale::for_divider` derives
/// both halves of the channel configuration from these two numbers — see
/// `BATTERY_DIVIDER`'s twin in `boards/solarnode.rs` for the argument and
/// the measurable range (10 660 mV full scale).
///
/// The pack is whatever the user solders to the BAT pad — the kit ships
/// none — so no cell count is asserted here either; `battery.rs`
/// classifies from the first reading.
pub const BATTERY_DIVIDER: leviculum_battery_scale::Divider =
    leviculum_battery_scale::Divider::new(1_000_000, 510_000);

// Charge control — the XIAO module's BQ25101, the one piece of power
// hardware upstream's `initVariant` touches besides the LEDs.
/// Charge-current select (`HICHG` = index 22 = P0.13), the BQ25101 ISET
/// pin: driven **LOW** for 100 mA instead of the 50 mA default
/// (`initVariant` writes it low before anything else).
///
/// We mirror upstream here rather than leaving the pin floating: a kit
/// with a battery on the pad charges at the rate the product intends,
/// and a kit with none pays nothing for the pin being driven.
pub type HighCharge = peripherals::P0_13;
/// Charge-detect (`EXT_CHRG_DETECT` = index 23 = P0.17, the charger
/// LED): `EXT_CHRG_DETECT_VALUE LOW` while the pack is charging.
///
/// Declared and deliberately not wired in: the firmware has no
/// charge-status surface for it to feed — the `BATTERY` line reports
/// volts, not state — so it stays an input until there is a reader.
pub type ChargeDetect = peripherals::P0_17;

/// Create the green LED output. **Active LOW**; off at boot means HIGH.
pub fn led(pin: Peri<'static, LedPin>) -> Output<'static> {
    Output::new(pin, Level::High, OutputDrive::Standard)
}

/// Create the blue LED output. **Active LOW**, same shape as [`led`].
/// Off at boot.
pub fn led_notification(pin: Peri<'static, LedNotificationPin>) -> Output<'static> {
    Output::new(pin, Level::High, OutputDrive::Standard)
}

/// Drive the BQ25101's ISET pin low — 100 mA charge current, the value
/// upstream's `initVariant` chooses. The returned `Output` must live for
/// the life of the program: dropping it floats the pin back to the
/// resistor-set default.
pub fn high_charge(pin: Peri<'static, HighCharge>) -> Output<'static> {
    Output::new(pin, Level::Low, OutputDrive::Standard)
}

/// Runtime board metadata for shared init code (USB / flash / LoRa).
///
/// The three persistence pages are the same three every board uses: they
/// are properties of the Adafruit bootloader's `USER_FLASH_END`
/// (`memory.x`), which this board shares, not of the module pair.
pub const CONFIG: super::BoardConfig = super::BoardConfig {
    usb_vid: 0x1209,
    usb_pid: 0x0004,
    usb_manufacturer: "leviculum",
    usb_product: "leviculum XiaoKit",
    log_prefix: "XIAO",
    identity_flash_page: 0xEC000,
    radio_config_flash_page: 0xEB000,
    telemetry_flash_page: 0xEA000,
    lora_tcxo_voltage_reg: 0x02, // 1.8 V
    lora_spi_freq_hz: LORA_SPI_FREQ_HZ,
    lora_max_power_dbm: LORA_MAX_POWER_DBM,
    // The kit ships the plain XIAO, whose own schematic marks U7 `DNP`
    // and whose variant comments the pins out. See the block above.
    qspi_part: None,
};

/// Panic-LED descriptor — port, pin, active-low flag — for `set_panic_led`.
/// Green LED on P0.30, **active low** (common-anode RGB).
pub const PANIC_LED_PORT: u8 = 0;
pub const PANIC_LED_PIN: u8 = 30;
pub const PANIC_LED_ACTIVE_LOW: bool = true;
