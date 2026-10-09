//! Seeed XIAO ESP32-S3 + Wio-SX1262 kit pin map and hardware constants.
//!
//! Two Seeed modules mated by the kit's B2B connector: a plain XIAO
//! ESP32-S3 (ESP32-S3R8, 8 MB flash, 8 MB PSRAM, USB-C on the SoC's own
//! USB Serial/JTAG pins) and the kit-specific Wio-SX1262 adapter. The
//! adapter is the point worth understanding: it is NOT the same
//! Wio-SX1262 board that fits the XIAO nRF52840 — Seeed's kit page says
//! the compatible radio board "can only be bought within the kit", and
//! the reason is visible in the pin map: half the radio nets land on the
//! four JTAG pads on the XIAO's underside (MTCK/MTDO/MTDI/MTMS =
//! GPIO39/40/41/42) plus the FSPIWP pad (GPIO38), which a plain header
//! footprint cannot reach.
//!
//! # Where these numbers come from
//!
//! Three independent sources agree on every radio net:
//!
//!  * the kit pinout published by Seeed
//!    (<https://wiki.seeedstudio.com/xiao_esp32s3_&_wio_SX1262_kit_for_meshtastic/>,
//!    B2B pin-mapping diagram),
//!  * the upstream Meshtastic variant
//!    `variants/esp32s3/seeed_xiao_s3/variant.h`,
//!  * the RadioMesh Zephyr overlay `xiao_esp32s3_procpu.overlay`, which
//!    carries the same numbers as raw GPIOs with the JTAG pads named.
//!
//! The module's nets are labelled by the pad they land on:
//!
//! | radio net | pad | GPIO |
//! |-----------|-----|------|
//! | NSS       | MTDI  | 41 |
//! | NRESET    | MTMS  | 42 |
//! | BUSY      | MTDO  | 40 |
//! | DIO1      | MTCK  | 39 |
//! | RF-SW     | FSPIWP | 38 |
//! | SCK       | D8    | 7  |
//! | MISO      | D9    | 8  |
//! | MOSI      | D10   | 9  |
//!
//! The nRF kit's sibling of this file is
//! `leviculum-nrf/src/boards/xiaokit.rs`; the two kits share the radio's
//! control model (external RX-enable plus DIO2 steering TX) and nothing
//! else — different SoC, different pads, different carrier.

use esp_hal::peripherals;

// ---------------------------------------------------------------------
// SX1262 LoRa radio, on SPI2
// ---------------------------------------------------------------------

/// SX1262 SPI chip-select, net `NSS` on the B2B connector (active low).
///
/// Lands on MTDI = GPIO41 — a JTAG pad on the board's underside, not a
/// header pin; only usable after the JTAG strapping releases it, which
/// happens at reset completion (the pads latch their strap values and are
/// then free IO).
pub type LoRaNss<'d> = peripherals::GPIO41<'d>;

/// SX1262 reset, net `RST` on MTMS = GPIO42 (active low).
pub type LoRaReset<'d> = peripherals::GPIO42<'d>;

/// SX1262 busy indicator, net `BUSY` on MTDO = GPIO40 (high = busy).
pub type LoRaBusy<'d> = peripherals::GPIO40<'d>;

/// SX1262 DIO1 interrupt output, net `DIO1` on MTCK = GPIO39.
pub type LoRaDio1<'d> = peripherals::GPIO39<'d>;

/// SX1262 SPI clock, net `SCK` on the D8 header pin = GPIO7.
pub type LoRaSck<'d> = peripherals::GPIO7<'d>;

/// SX1262 SPI MOSI, net `MOSI` on the D10 header pin = GPIO9.
pub type LoRaMosi<'d> = peripherals::GPIO9<'d>;

/// SX1262 SPI MISO, net `MISO` on the D9 header pin = GPIO8.
pub type LoRaMiso<'d> = peripherals::GPIO8<'d>;

/// Antenna-switch receive enable, net `RF-SW` on the FSPIWP pad = GPIO38.
///
/// Host-driven, like the nRF kit's `P0.05`: assert it for a listening
/// window, release it before key-up. The TX side is steered by the
/// SX1262's own DIO2 (`SX126X_DIO2_AS_RF_SWITCH` upstream), so the front
/// end is the same two-sided arrangement as the nRF kit — one host pin
/// for RX, one radio pin for TX.
///
/// Upstream's variant header writes this pin twice — `LORA_DIO2 38` and
/// `SX126X_RXEN 38` — which reads as one physical net claimed by two
/// names. Seeed's pinout labels the pad `RF-SW`, and the Zephyr overlay
/// resolves the ambiguity the same way we do: DIO2 stays the chip's
/// internal TX steering, GPIO38 is the external RX-enable.
pub type LoRaRxEnable<'d> = peripherals::GPIO38<'d>;

/// SX1262 SPI bus frequency in Hz.
///
/// Same conservative starting point as the V4: the SX1262 datasheet
/// allows 16 MHz, and raising a bus that has never run on this board is a
/// measurement, not an edit.
pub const LORA_SPI_FREQ_HZ: u32 = 8_000_000;

/// SX1262 maximum TX power in dBm at the chip's own output.
///
/// This kit is the bare SX1262 — there is no PA between the chip and the
/// antenna, so unlike the V4 the datasheet number and the air number are
/// the same thing.
pub const LORA_MAX_POWER_DBM: i8 = 22;

/// The 32 MHz TCXO on the Wio-SX1262 is supplied from the SX1262's DIO3,
/// so the firmware must issue `SetDIO3AsTcxoCtrl` before calibration
/// (upstream `SX126X_DIO3_TCXO_VOLTAGE`).
pub const LORA_TCXO_ON_DIO3: bool = true;

/// The DIO3 output voltage the kit's TCXO needs: **1.8 V**.
///
/// Established, unlike the V4's: the Meshtastic variant states
/// `SX126X_DIO3_TCXO_VOLTAGE 1.8`, the same value the nRF kit's variant
/// states for the sibling Wio module, and no source disagrees.
pub const LORA_TCXO_VOLTAGE_1V8: () = ();

// ---------------------------------------------------------------------
// Indicators
// ---------------------------------------------------------------------

/// User LED, the yellow `USER_LED` on the XIAO ESP32-S3: **GPIO21, active
/// LOW** (the LED only lights with the pin driven low; Seeed's board
/// pinout and the NuttX board docs agree, and the Arduino core's
/// `LED_BUILTIN` is 21).
///
/// Upstream's `LED_POWER 48` is not this net: GPIO48 on the plain S3 is
/// not an LED (on the Sense it is a camera pin), and the upstream
/// `pins_arduino.h` contradicts the same file by mapping SCL onto 48.
/// We take the board schematic's LED, not the variant header's.
pub type LedPin<'d> = peripherals::GPIO21<'d>;

/// False: [`LedPin`] is wired to conduct toward the pin, so LOW lights it.
pub const LED_ACTIVE_HIGH: bool = false;

// Buttons — the XIAO ESP32-S3's BOOT button is the ROM download strap, not
// a general input. Upstream maps `BUTTON_PIN 21`, which is the LED's net,
// so the button pin is deliberately not declared: nothing needs it, and
// carrying a wrong pin is worse than carrying none.

// ---------------------------------------------------------------------
// Battery — none
// ---------------------------------------------------------------------

// The kit carries no battery divider: the XIAO's BAT pads are a charge
// input, not a sensed rail (upstream `BATTERY_PIN -1`), so there is no
// ADC pin to declare.

// ---------------------------------------------------------------------
// Optional L76K GNSS (add-on, not part of the kit)
// ---------------------------------------------------------------------

/// MCU TX toward the L76K's RX, on the D6 header pin = GPIO43.
///
/// Same naming convention as the nRF kit (`GPS_TX_PIN` names the wire the
/// MCU's data leaves on). UART0's console pads are exactly these two
/// pins; with the console on USB Serial/JTAG they are free to carry GNSS.
pub type GnssTx<'d> = peripherals::GPIO43<'d>;

/// MCU RX from the L76K's TX, on the D7 header pin = GPIO44
/// (upstream `GPS_RX_PIN`).
pub type GnssRx<'d> = peripherals::GPIO44<'d>;

/// L76K standby, on the D0 header pin = GPIO1 (upstream `PIN_GPS_STANDBY`
/// — the same pad the nRF kit wires to its standby, by XIAO D-label
/// rather than GPIO number coincidence).
pub type GnssStandby<'d> = peripherals::GPIO1<'d>;

/// The kit ships no GNSS and no enable line for one — an L76K that is
/// plugged in is simply powered, like on the nRF kit.
pub const GNSS_ONBOARD: bool = false;

// ---------------------------------------------------------------------
// Header I2C (D4/D5)
// ---------------------------------------------------------------------

/// I2C data on the D4 header pin = GPIO5 (upstream `I2C_SDA`, and the
/// XIAO's default `Wire` SDA).
pub type I2cSda<'d> = peripherals::GPIO5<'d>;
/// I2C clock on the D5 header pin = GPIO6 (upstream `I2C_SCL`).
pub type I2cScl<'d> = peripherals::GPIO6<'d>;

// ---------------------------------------------------------------------
// PID loop: DS18B20 + valve (D1/D2)
// ---------------------------------------------------------------------

/// 1-Wire data on the D1 header pin = GPIO2 — the DS18B20's DQ, with an
/// external 4.7 kΩ pull-up. Open-drain in both directions; bit-banged,
/// no peripheral involved.
pub type OnewireData<'d> = peripherals::GPIO2<'d>;
/// Valve drive on the D3 header pin = GPIO4 — high opens. D2 is skipped
/// deliberately: it is GPIO3, a strapping pin, and a valve that pulls
/// the line at reset would write itself into the boot mode. A relay
/// coil or MOSFET gate is a board-externals problem, not this pin's.
pub type Valve<'d> = peripherals::GPIO4<'d>;

// USB is not a pin alias here either: the Type-C connector runs straight
// to GPIO19 (D-) and GPIO20 (D+), the SoC's USB Serial/JTAG peripheral —
// `lib.rs` opens it through `peripherals.USB_DEVICE`. Same topology as
// the V4.

/// The board's entry in the shared metadata table.
pub const CONFIG: super::BoardConfig = super::BoardConfig {
    log_prefix: "XS3",
    board_name: "Seeed XIAO ESP32-S3 + Wio-SX1262",
    lora_spi_freq_hz: LORA_SPI_FREQ_HZ,
    lora_max_power_dbm: LORA_MAX_POWER_DBM,
    gnss_onboard: GNSS_ONBOARD,
};
