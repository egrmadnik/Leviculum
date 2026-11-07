//! Firmware binary for the Seeed XIAO ESP32-S3 + Wio-SX1262 kit.
//!
//! This file is the board's hardware definition: its pin map, its LoRa
//! channel and the polarity of its LED. Everything else an LNode does is
//! `leviculum_esp::node`: the boot banner, the SX1262 carrier, the
//! Reticulum node, the add-on sensors, the cooperative loop, and the app
//! that runs on top (the PID valve regulator, or the LXMF echo inbox with
//! `--features lxmf-echo`).
//!
//! The pin handles are moved out of `Peripherals` through the board
//! module's type aliases rather than by their GPIO numbers. That is not
//! decoration: `let nss: xiao_s3::LoRaNss = p.GPIO41` is a compile-time
//! assertion that this binary and the board file agree about which pin
//! carries `NSS`, and it is the only place the two can be held to each
//! other.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use esp_hal::gpio::Pin;
use esp_hal::peripherals::Peripherals;
use leviculum_esp::boards::xiao_s3;
use leviculum_esp::node::board::{
    AppWiring, Board, LoraProfile, LoraWiring, SensorWiring, System, Wiring,
};

// The application descriptor the second-stage bootloader reads. Without it
// `espflash save-image` produces an image the ROM declines.
esp_bootloader_esp_idf::esp_app_desc!();

/// The Seeed XIAO ESP32-S3 + Wio-SX1262 kit.
struct XiaoS3;

impl Board for XiaoS3 {
    const NAME: &'static str = "XIAO ESP32-S3";
    const CONFIG: &'static leviculum_esp::boards::BoardConfig = &xiao_s3::CONFIG;
    /// The `eu_medium` numbers the fleet agrees on (ReticulumNet consensus
    /// channel, SF8/BW125/CR4/5, the 18-symbol preamble
    /// `derive_preamble_symbols` lands on).
    const LORA: LoraProfile = LoraProfile {
        freq_hz: 869_463_000,
        sf: 8,
        bw_code: 0x04,
        bw_hz: 125_000,
        cr_code: 0x01,
        cr_denominator: 5,
        preamble_symbols: 18,
        power_dbm: 22,
    };
    // The module's yellow USER_LED on GPIO21, **active low**.
    const LED_ACTIVE_HIGH: bool = xiao_s3::LED_ACTIVE_HIGH;

    fn wire(p: Peripherals) -> Wiring {
        let led: xiao_s3::LedPin = p.GPIO21;

        // The SX1262's port.
        let sck: xiao_s3::LoRaSck = p.GPIO7;
        let mosi: xiao_s3::LoRaMosi = p.GPIO9;
        let miso: xiao_s3::LoRaMiso = p.GPIO8;
        let nss: xiao_s3::LoRaNss = p.GPIO41;
        let busy: xiao_s3::LoRaBusy = p.GPIO40;
        let reset: xiao_s3::LoRaReset = p.GPIO42;
        let dio1: xiao_s3::LoRaDio1 = p.GPIO39;
        let rxen: xiao_s3::LoRaRxEnable = p.GPIO38;

        // The Qwiic/I2C pads (D4/D5): an add-on BMV080 or BME690.
        let sda: xiao_s3::I2cSda = p.GPIO5;
        let scl: xiao_s3::I2cScl = p.GPIO6;

        // The PID app's pins: DS18B20 on D1 (open-drain, external 4.7 kΩ
        // pull-up), the valve on D3.
        let onewire: xiao_s3::OnewireData = p.GPIO2;
        let valve: xiao_s3::Valve = p.GPIO4;

        Wiring {
            system: System {
                usb: p.USB_DEVICE,
                flash: p.FLASH,
                rng: p.RNG,
                adc1: p.ADC1,
                timg0: p.TIMG0,
                sw_int0: p.FROM_CPU_INTR0,
            },
            led: led.degrade(),
            lora: LoraWiring {
                spi: p.SPI2,
                sck: sck.degrade(),
                mosi: mosi.degrade(),
                miso: miso.degrade(),
                nss: nss.degrade(),
                busy: busy.degrade(),
                reset: reset.degrade(),
                dio1: dio1.degrade(),
                rxen: rxen.degrade(),
                spi_freq_hz: xiao_s3::LORA_SPI_FREQ_HZ,
            },
            sensors: SensorWiring {
                i2c: p.I2C0,
                sda: sda.degrade(),
                scl: scl.degrade(),
            },
            app: AppWiring {
                onewire: onewire.degrade(),
                valve: valve.degrade(),
                led: None,
                led_active_high: false,
            },
        }
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    leviculum_esp::node::run::<XiaoS3>(spawner).await
}
