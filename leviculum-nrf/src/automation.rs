//! `leviculum_automation::hal` over the nRF52840 kit — the counterpart
//! of `leviculum-esp/src/automation.rs`, so the same `App<D>` runs on
//! both boards.
//!
//! Four adaptors here; the sensor is the one a device picks itself (a
//! DS18B20 on a GPIO, an I2C probe, an ADC channel), so it stays in the
//! binary. What this file settles is everything that is *the board*:
//! how an output pin is driven, how a page of flash is read and
//! written, what the clock is, where a log line goes.
//!
//! # Flash on this chip
//!
//! [`NvmcStore`] writes through `embassy_nrf::nvmc::Nvmc` — the direct
//! controller, which is **only legal while the SoftDevice is not
//! running** (the SoftDevice owns NVMC once enabled; a direct write
//! then is a hard fault). Two correct uses:
//!
//! - a `no-softdevice` build (no BLE), where NVMC is yours throughout;
//! - a SoftDevice build that persists only *before* `Softdevice::enable`
//!   — i.e. a params store that is read at boot and written through the
//!   shared-flash task (`crate::flash::shared_flash`) after that.
//!
//! The second is the pattern `radio_store` uses: `load` is synchronous
//! NVMC at boot, `save` is a request to an async task holding the
//! SoftDevice flash. An `App` on a BLE build should route
//! `Inbound::Applied { blob, .. }` to such a task rather than call
//! `ConfigStore::save` from the loop. The trait makes that a one-line
//! swap in the wrapper, which is the point of the trait.

extern crate alloc;

use alloc::vec::Vec;
use embassy_nrf::gpio::{Level, Output};
use embassy_nrf::nvmc::Nvmc;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use leviculum_automation::hal::{Actuator, Clock, ConfigStore, Log};

/// A push-pull output, high = on.
pub struct GpioActuator<'d>(pub Output<'d>);

impl Actuator for GpioActuator<'_> {
    fn set(&mut self, on: bool) {
        self.0.set_level(if on { Level::High } else { Level::Low });
    }
}

/// One 4 KiB NVMC page: `[len: u16 LE][blob][crc32 of len+blob]` — the
/// identical layout to the ESP store, so a blob is a blob on either.
pub struct NvmcStore<'d> {
    pub nvmc: Nvmc<'d>,
    pub page: u32,
}

const PAGE: u32 = 4096;

impl ConfigStore for NvmcStore<'_> {
    fn load(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; 4];
        self.nvmc.read(self.page, &mut hdr).ok()?;
        let len = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
        if len == 0 || len > 1024 {
            return None;
        }
        // NVMC reads are word-granular; round the buffer up.
        let total = 2 + len + 4;
        let mut buf = alloc::vec![0u8; (total + 3) & !3];
        self.nvmc.read(self.page, &mut buf).ok()?;
        let (body, crc) = buf[..total].split_at(2 + len);
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
        let mut buf = Vec::with_capacity(2 + blob.len() + 8);
        buf.extend_from_slice(&(blob.len() as u16).to_le_bytes());
        buf.extend_from_slice(blob);
        let crc = leviculum_automation::frame::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        while buf.len() % 4 != 0 {
            buf.push(0xFF);
        }
        self.nvmc.erase(self.page, self.page + PAGE).is_ok()
            && self.nvmc.write(self.page, &buf).is_ok()
    }
}

/// `embassy_time::Instant` as the app's clock.
pub struct EmbassyClock;

impl Clock for EmbassyClock {
    fn now_ms(&self) -> u64 {
        embassy_time::Instant::now().as_millis()
    }
}

/// The board's log ring as the app's sink.
pub struct FirmwareLog;

impl Log for FirmwareLog {
    fn line(&mut self, tag: &str, args: core::fmt::Arguments<'_>) {
        crate::log::log_fmt(tag, args);
    }
}
