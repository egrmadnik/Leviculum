//! DS18B20 1-Wire temperature sensor — bit-banged on one GPIO.
//!
//! The bus is Maxim's single-wire protocol: one open-drain line with a
//! 4.7 kΩ pull-up, reset/presence handshake, then ROM + function
//! commands. This driver assumes exactly one device on the bus, so the
//! ROM phase is always `SKIP ROM` — no search, no addressing. A kit
//! that ever hangs two sensors on one pin is the case that adds it.
//!
//! Timings are the standard ones: 480 µs reset low, ~70 µs to the
//! presence sample, 60 µs write-0 slot / ~6 µs write-1 slot, reads
//! sampled ~12 µs into the slot, 70 µs slot with recovery between.
//! `Delay` here is esp-hal's cycle-counted delay — µs-accurate, which
//! is what the slots ask for.

use esp_hal::{delay::Delay, gpio::Flex};

// 1-Wire function commands (Maxim DS18B20 datasheet table).
const CMD_SKIP_ROM: u8 = 0xcc;
const CMD_CONVERT_T: u8 = 0x44;
const CMD_READ_SCRATCHPAD: u8 = 0xbe;

/// The conversion wait at the sensor's default 12-bit resolution —
/// the datasheet's `T_conv` maximum.
const CONVERT_MS_12BIT: u32 = 750;

/// One DS18B20 on a bit-banged bus.
pub struct Ds18b20<'d> {
    pin: Flex<'d>,
    delay: Delay,
}

impl<'d> Ds18b20<'d> {
    /// Wrap an already-configured `Flex` pin — the caller set
    /// open-drain + input-enable and released it high, i.e. the board
    /// file owns the electrical setup.
    pub fn new(pin: Flex<'d>) -> Self {
        Self {
            pin,
            delay: Delay::new(),
        }
    }

    /// Reset pulse → presence pulse. `false` is *no sensor answered* —
    /// a bare line pulls high; an answered presence pulls low.
    fn reset(&mut self) -> bool {
        self.pull_low(480);
        self.release();
        self.delay.delay_micros(70);
        let present = self.pin.is_low();
        self.delay.delay_micros(410);
        present
    }

    /// One 1-Wire byte, LSB first — the bus order.
    fn write_byte(&mut self, byte: u8) {
        for bit in 0..8 {
            if byte >> bit & 1 == 1 {
                // write-1 slot: short low, line released for the rest.
                self.pull_low(6);
                self.release();
                self.delay.delay_micros(64);
            } else {
                // write-0 slot: held low through the slot.
                self.pull_low(60);
                self.release();
                self.delay.delay_micros(10);
            }
        }
    }

    /// One 1-Wire byte read, LSB first.
    fn read_byte(&mut self) -> u8 {
        let mut byte = 0u8;
        for bit in 0..8 {
            // Read slot: low ≥1 µs, release, the sensor's data is the
            // level sampled ~12 µs in.
            self.pin.set_low();
            self.pin.set_output_enable(true);
            self.delay.delay_micros(3);
            self.pin.set_output_enable(false);
            self.delay.delay_micros(9);
            if self.pin.is_high() {
                byte |= 1 << bit;
            }
            self.delay.delay_micros(55);
        }
        byte
    }

    fn pull_low(&mut self, us: u32) {
        self.pin.set_low();
        self.pin.set_output_enable(true);
        self.delay.delay_micros(us);
    }

    /// Released = open drain: the pull-up drives the line high, the
    /// sensor may answer.
    fn release(&mut self) {
        self.pin.set_output_enable(false);
    }

    /// `true` if a sensor answered the reset — the boot probe, same
    /// `state=present|absent` discipline as the I2C sensors.
    pub fn present(&mut self) -> bool {
        self.reset()
    }

    /// Start a conversion. The sensor is left powered; at 12 bits it is
    /// done within 750 ms — [`Self::read_scratchpad`] before that returns
    /// `None` (the scratchpad still holds the last conversion, which is
    /// a stale read, not an error).
    pub fn start_convert(&mut self) -> bool {
        if !self.reset() {
            return false;
        }
        self.write_byte(CMD_SKIP_ROM);
        self.write_byte(CMD_CONVERT_T);
        true
    }

    /// Read the scratchpad after a conversion and return °C.
    ///
    /// `None` covers both failure modes: no sensor present, and a
    /// scratchpad that fails its CRC8 — a corrupted frame is dropped
    /// rather than reported as a temperature.
    pub fn read_scratchpad(&mut self) -> Option<f32> {
        if !self.reset() {
            return None;
        }
        self.write_byte(CMD_SKIP_ROM);
        self.write_byte(CMD_READ_SCRATCHPAD);
        let mut sp = [0u8; 9];
        for b in &mut sp {
            *b = self.read_byte();
        }
        if crc8(&sp[..8]) != sp[8] {
            return None;
        }
        // DS18B20 T: 16-bit signed, 12-bit fraction — raw/16 °C.
        let raw = i16::from_le_bytes([sp[0], sp[1]]);
        Some(raw as f32 / 16.0)
    }

    /// Full cycle: convert, wait, read. `None` on absent or CRC fail.
    pub fn measure(&mut self) -> Option<f32> {
        if !self.start_convert() {
            return None;
        }
        self.delay.delay_millis(CONVERT_MS_12BIT);
        self.read_scratchpad()
    }
}

/// Dallas/Maxim CRC8 — poly 0x31 reflected, init 0. Table-less; nine
/// bytes a read makes the table never worth it.
fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        let mut mix = crc ^ b;
        for _ in 0..8 {
            mix = if mix & 1 == 1 {
                (mix >> 1) ^ 0x8c
            } else {
                mix >> 1
            };
        }
        crc = mix;
    }
    crc
}
