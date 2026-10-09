//! `leviculum_automation::hal` over the XIAO ESP32-S3's peripherals —
//! the whole MCU-specific surface of an automation device, in one file.
//!
//! Five adaptors, none with logic of its own: the DS18B20 driver as a
//! [`Sensor`], a GPIO as an [`Actuator`], a flash sector as a
//! [`ConfigStore`], `uptime_ms` as a [`Clock`], the USB port as a
//! [`Log`]. The nRF wrapper is the same five over embassy-nrf.

use alloc::vec::Vec;
use esp_hal::gpio::{Level, Output};
use leviculum_automation::hal::{Actuator, Clock, ConfigStore, Log, Sensor};

use crate::ds18b20::Ds18b20;
use crate::UsbLog;

extern crate alloc;

/// DS18B20 on 1-Wire: one blocking convert+read per `read`.
pub struct Ds18b20Sensor<'d>(pub Ds18b20<'d>);

impl Sensor for Ds18b20Sensor<'_> {
    fn read(&mut self) -> Option<f32> {
        self.0.measure()
    }
}

/// A push-pull output, high = on.
pub struct GpioActuator<'d>(pub Output<'d>);

impl Actuator for GpioActuator<'_> {
    fn set(&mut self, on: bool) {
        self.0.set_level(if on { Level::High } else { Level::Low });
    }
}

/// One 4 KiB sector: `[len: u16 LE][blob][crc32 of len+blob]`. The
/// blob's own frame CRC already refuses corruption; the outer CRC
/// exists so a half-written sector reads as `None` rather than as a
/// short blob.
pub struct FlashStore<'d> {
    pub flash: esp_storage::FlashStorage<'d>,
    pub offset: u32,
}

const SECTOR: u32 = 4096;

impl ConfigStore for FlashStore<'_> {
    fn load(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; 2];
        self.flash.read(self.offset, &mut hdr).ok()?;
        let len = u16::from_le_bytes(hdr) as usize;
        if len == 0 || len > 1024 {
            return None;
        }
        let mut buf = alloc::vec![0u8; 2 + len + 4];
        self.flash.read(self.offset, &mut buf).ok()?;
        let (body, crc) = buf.split_at(2 + len);
        if leviculum_automation::frame::crc32(body)
            != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]])
        {
            return None;
        }
        Some(body[2..].to_vec())
    }

    fn save(&mut self, blob: &[u8]) -> bool {
        if blob.len() > 1024 {
            return false;
        }
        let mut buf = Vec::with_capacity(2 + blob.len() + 4);
        buf.extend_from_slice(&(blob.len() as u16).to_le_bytes());
        buf.extend_from_slice(blob);
        let crc = leviculum_automation::frame::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        self.flash.erase(self.offset, self.offset + SECTOR).is_ok()
            && self.flash.write(self.offset, &buf).is_ok()
    }
}

/// `uptime_ms` as the app's clock.
pub struct UptimeClock;

impl Clock for UptimeClock {
    fn now_ms(&self) -> u64 {
        crate::uptime_ms()
    }
}

impl Log for UsbLog<'_> {
    fn line(&mut self, tag: &str, args: core::fmt::Arguments<'_>) {
        UsbLog::line(self, tag, args);
    }
}
