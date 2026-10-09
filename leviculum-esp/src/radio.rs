//! The Wio-SX1262 as a Reticulum interface — the chip driver, not the
//! bus.
//!
//! This is the `xiao_s3` answer to `leviculum-nrf/src/sx1262.rs`, cut to
//! what a polled blocking loop needs: init, configure, transmit, and a
//! non-blocking receive check. The sequence ordering is the nRF
//! driver's, and both sit on the same `leviculum_core::sx126x`
//! sequences (`program_tx_power`, `probe_rx_init`, `apply_iq_polarity`)
//! where the ordering is what has to match the datasheet, so the two
//! radios agree at the level a host test can see.
//!
//! # Simplifications versus the nRF driver
//!
//! - **Blocking**: `block_on` spins on the bus futures instead of
//!   running an embassy task with DIO interrupts — one core, one loop,
//!   nothing to wake.
//! - **Continuous RX**: `SetRx(0xFFFFFF)` once, and the chip stays
//!   listening after every `RxDone` — no arm/disarm window bookkeeping,
//!   no `RxArmClock` instrumentation. The cost is honesty: there is no
//!   dark-time measurement, because there is no dark time to measure
//!   when the receiver is never stood down except to transmit.
//! - **No CAD**: carrier-sense defers to the mesh being small. A TX
//!   while a preamble is inbound loses the inbound frame — the real
//!   fix, `sx126x::cad_params` + `SetCad`, is the same call the nRF
//!   driver makes and belongs on this list when the mesh gets busy.

use esp_hal::{
    gpio::{Input, Output},
    time::{Duration, Instant},
};
use leviculum_core::sx126x;
use leviculum_core::sx126x::{CommandBus, RegisterBus};

use crate::sx1262::Sx1262Bus;

/// `uptime_ms()`-based spin — used at init only, where the caller owns
/// the chip's settling times and there is nothing else to poll.
fn delay_ms(ms: u64) {
    let end = crate::uptime_ms() + ms;
    while crate::uptime_ms() < end {
        core::hint::spin_loop();
    }
}

/// The opcodes this driver issues. Multi-byte register access goes
/// through the bus (`RegisterBus`/`CommandBus`); these are the
/// command-layer opcodes that the sequences in `sx126x` do not already
/// cover — same table, same values as the nRF driver's `mod opcode`.
mod op {
    pub const GET_STATUS: u8 = 0xC0;
    pub const GET_IRQ_STATUS: u8 = 0x12;
    pub const CLEAR_IRQ_STATUS: u8 = 0x02;
    pub const SET_DIO_IRQ_PARAMS: u8 = 0x08;
    pub const SET_STANDBY: u8 = 0x80;
    pub const SET_TX: u8 = 0x83;
    pub const SET_RX: u8 = 0x82;
    pub const SET_REGULATOR_MODE: u8 = 0x96;
    pub const SET_DIO3_AS_TCXO_CTRL: u8 = 0x97;
    pub const CLEAR_DEVICE_ERRORS: u8 = 0x07;
    pub const CALIBRATE: u8 = 0x89;
    pub const CALIBRATE_IMAGE: u8 = 0x98;
    pub const SET_DIO2_AS_RF_SWITCH: u8 = 0x9D;
    pub const SET_PACKET_TYPE: u8 = 0x8A;
    pub const SET_RF_FREQUENCY: u8 = 0x86;
    pub const SET_BUFFER_BASE_ADDRESS: u8 = 0x8F;
    pub const SET_MODULATION_PARAMS: u8 = 0x8B;
    pub const SET_PACKET_PARAMS: u8 = 0x8C;
    pub const GET_RX_BUFFER_STATUS: u8 = 0x13;
    pub const GET_PACKET_STATUS: u8 = 0x14;
    pub const SET_STOP_RX_TIMER_ON_PREAMBLE: u8 = 0x9F;
}

mod reg {
    pub const LORA_SYNC_WORD: u16 = 0x0740;
    pub const TX_CLAMP_CONFIG: u16 = 0x08D8;
}

/// Run a bus future to completion on this blocking core: a noop waker
/// and a spin — esp-hal's async SPI completes through its ISR, so
/// pending polls resolve.
fn block_on<F: core::future::Future>(mut f: F) -> F::Output {
    use core::task::{RawWaker, RawWakerVTable, Waker};
    const VT: RawWakerVTable = RawWakerVTable::new(|_| RAW_WAKER, |_| {}, |_| {}, |_| {});
    const RAW_WAKER: RawWaker = RawWaker::new(core::ptr::null(), &VT);
    // SAFETY: the vtable's fns never dereference the null data pointer.
    let waker = unsafe { Waker::from_raw(RAW_WAKER) };
    let mut f = unsafe { core::pin::Pin::new_unchecked(&mut f) };
    let mut cx = core::task::Context::from_waker(&waker);
    loop {
        if let core::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        core::hint::spin_loop();
    }
}

/// Air-side result of one received frame.
#[derive(Debug, Clone, Copy)]
pub struct RxStatus {
    pub rssi: i16,
    pub snr: i16,
}

/// What `poll_rx` found.
pub enum Rx {
    /// A frame that passed its payload CRC.
    Frame { len: u8, status: RxStatus },
    /// Header decoded, payload CRC failed — the frame is gone but the
    /// event is real.
    BadCrc,
    /// Nothing terminating.
    Idle,
}

/// Radio bring-up and frame I/O over [`Sx1262Bus`], plus the three
/// GPIOs the protocol owns: reset, the DIO1 terminating-IRQ line, and
/// the kit's RF switch (high = RX path; DIO2 steers the TX side inside
/// the chip).
pub struct Radio<'d, SPI> {
    pub bus: Sx1262Bus<'d, SPI>,
    rst: Output<'d>,
    dio1: Input<'d>,
    rfsw: Output<'d>,
    preamble_len: u16,
}

impl<'d, SPI: embedded_hal_async::spi::SpiBus> Radio<'d, SPI> {
    pub fn new(
        bus: Sx1262Bus<'d, SPI>,
        rst: Output<'d>,
        dio1: Input<'d>,
        rfsw: Output<'d>,
    ) -> Self {
        Self {
            bus,
            rst,
            dio1,
            rfsw,
            preamble_len: 8,
        }
    }

    fn cmd(&mut self, op: u8, args: &[u8]) -> Result<(), crate::sx1262::Error> {
        block_on(self.bus.write_cmd(op, args))
    }

    fn read(&mut self, op: u8, out: &mut [u8]) -> Result<(), crate::sx1262::Error> {
        block_on(self.bus.read_cmd(op, out)).map(|_| ())
    }

    fn reg_write(&mut self, addr: u16, val: u8) -> Result<(), crate::sx1262::Error> {
        block_on(self.bus.write_reg(addr, val))
    }

    fn reg_read(&mut self, addr: u16) -> Result<u8, crate::sx1262::Error> {
        block_on(self.bus.read_reg(addr))
    }

    /// Standby + RF switch to the RX-off position. Every path that
    /// leaves RX goes through here — the line is released before the
    /// command, so a failed standby cannot leave it selecting.
    fn standby(&mut self) -> Result<(), crate::sx1262::Error> {
        self.rfsw.set_low();
        self.cmd(op::SET_STANDBY, &[0x00])
    }

    /// Cold boot → standby, following the datasheet §9.2.1 order the
    /// nRF driver runs: regulator, RF switch, clear errors, TCXO,
    /// calibrate, image calibrate, packet type, boosted RX gain.
    pub fn init(&mut self, freq_hz: u32) -> Result<u8, crate::sx1262::Error> {
        // Hardware reset: 1 ms low is the datasheet's minimum (§13.3.1).
        self.rst.set_low();
        delay_ms(2);
        self.rst.set_high();
        delay_ms(10);

        self.cmd(op::SET_STANDBY, &[0x00])?;
        self.cmd(op::SET_REGULATOR_MODE, &[0x00])?; // LDO
        self.cmd(op::SET_DIO2_AS_RF_SWITCH, &[0x01])?;
        self.cmd(op::CLEAR_DEVICE_ERRORS, &[0x00, 0x00])?;
        // TCXO on DIO3, 1.8 V (reg value 0x02), ~4 ms to settle —
        // datasheet §9.2.1 wants calibration AFTER this.
        self.cmd(op::SET_DIO3_AS_TCXO_CTRL, &[0x02, 0x00, 0x00, 0xFF])?;
        delay_ms(10);
        self.cmd(op::CALIBRATE, &[0x7F])?;
        delay_ms(10);
        let (f1, f2) = if freq_hz > 900_000_000 {
            (0xE1, 0xE9)
        } else if freq_hz > 850_000_000 {
            (0xD7, 0xDB)
        } else if freq_hz > 770_000_000 {
            (0xC1, 0xC5)
        } else if freq_hz > 460_000_000 {
            (0x75, 0x81)
        } else {
            (0x6B, 0x6F)
        };
        self.cmd(op::CALIBRATE_IMAGE, &[f1, f2])?;
        delay_ms(10);
        self.cmd(op::SET_PACKET_TYPE, &[0x01])?; // LoRa
        let probe = block_on(sx126x::probe_rx_init(&mut self.bus))?;
        // The probe reports before/after — the caller logs it.
        let _ = probe;
        let mut status = [0u8; 1];
        self.read(op::GET_STATUS, &mut status)?;
        Ok(status[0])
    }

    /// Apply the Reticulum LoRa profile (the `eu_medium` numbers the
    /// nRF kit compiles in): frequency, PA, modulation, packet shape,
    /// private sync word, clamp workaround, preamble-detect timer.
    /// Ends listening.
    pub fn configure(
        &mut self,
        freq_hz: u32,
        sf: u8,
        bw: u8,
        cr: u8,
        power_dbm: i8,
        preamble_len: u16,
    ) -> Result<sx126x::TxPowerProgram, crate::sx1262::Error> {
        self.preamble_len = preamble_len;
        self.standby()?;
        let rf_freq = ((freq_hz as u64 * (1u64 << 25)) / 32_000_000) as u32;
        self.cmd(op::SET_RF_FREQUENCY, &rf_freq.to_be_bytes())?;
        let program = block_on(sx126x::program_tx_power(&mut self.bus, power_dbm))?;
        self.cmd(op::SET_BUFFER_BASE_ADDRESS, &[0x00, 0x00])?;
        // The register code is not a Hz table index — the nRF driver's
        // `bw_code_to_hz` mapping, transcribed.
        let bw_hz = match bw {
            0x00 => 7_810,
            0x08 => 10_420,
            0x01 => 15_630,
            0x09 => 20_830,
            0x02 => 31_250,
            0x0A => 41_670,
            0x03 => 62_500,
            0x04 => 125_000,
            0x05 => 250_000,
            0x06 => 500_000,
            _ => 0,
        };
        let ldro = sx126x::ldro_enabled(bw_hz, sf) as u8;
        self.cmd(op::SET_MODULATION_PARAMS, &[sf, bw, cr, ldro, 0, 0, 0, 0])?;
        self.packet_params(0xFF)?;
        // Private sync word — the Reticulum pair, not LoRaWAN's.
        self.reg_write(reg::LORA_SYNC_WORD, 0x14)?;
        self.reg_write(reg::LORA_SYNC_WORD + 1, 0x24)?;
        // Datasheet §15.2 TX PA clamp.
        let clamp = self.reg_read(reg::TX_CLAMP_CONFIG)? | 0x1E;
        self.reg_write(reg::TX_CLAMP_CONFIG, clamp)?;
        // StopRxTimerOnPreambleDetect: a standing timeout must not
        // abort a slow-SF frame mid-packet.
        self.cmd(op::SET_STOP_RX_TIMER_ON_PREAMBLE, &[0x01])?;
        Ok(program)
    }

    /// `SetPacketParams` + the erratum-15.4 IQ-polarity correction,
    /// every call — same rule the nRF driver keeps.
    fn packet_params(&mut self, payload_len: u8) -> Result<(), crate::sx1262::Error> {
        self.cmd(
            op::SET_PACKET_PARAMS,
            &[
                (self.preamble_len >> 8) as u8,
                self.preamble_len as u8,
                0x00,
                payload_len,
                0x01,
                0x00,
            ],
        )?;
        block_on(sx126x::apply_iq_polarity(&mut self.bus, false, false)).map(|_| ())
    }

    /// Put the receiver on the air, continuous.
    pub fn listen(&mut self) -> Result<(), crate::sx1262::Error> {
        self.cmd(op::SET_DIO_IRQ_PARAMS, &sx126x::rx_irq_params())?;
        self.cmd(op::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
        self.rfsw.set_high();
        self.cmd(op::SET_RX, &[0xFF, 0xFF, 0xFF])
    }

    /// Non-blocking RX check: `Idle` until DIO1 latches, then the
    /// terminating IRQ decides — a good frame, or the CRC-fail that is
    /// still evidence the channel was occupied.
    ///
    /// Continuous `SetRx` keeps the chip listening after `RxDone`, so
    /// there is nothing to re-arm.
    pub fn poll_rx(&mut self, buf: &mut [u8]) -> Result<Rx, crate::sx1262::Error> {
        if !self.dio1.is_high() {
            return Ok(Rx::Idle);
        }
        let mut irq = [0u8; 2];
        self.read(op::GET_IRQ_STATUS, &mut irq)?;
        let flags = u16::from_be_bytes(irq);
        self.cmd(op::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
        if flags & sx126x::IRQ_CRC_ERR != 0 {
            return Ok(Rx::BadCrc);
        }
        if flags & sx126x::IRQ_RX_DONE == 0 {
            return Ok(Rx::Idle);
        }
        let mut bstat = [0u8; 2];
        self.read(op::GET_RX_BUFFER_STATUS, &mut bstat)?;
        let len = bstat[0].min(buf.len() as u8);
        block_on(self.bus.read_payload(bstat[1], &mut buf[..len as usize]))?;
        let mut pstat = [0u8; 3];
        self.read(op::GET_PACKET_STATUS, &mut pstat)?;
        let (rssi, snr) = sx126x::packet_status_dbm(pstat);
        Ok(Rx::Frame {
            len,
            status: RxStatus { rssi, snr },
        })
    }

    /// Blocking transmit. Leaves RX first; on return the chip is in
    /// standby and the caller re-arms with [`listen`](Self::listen) —
    /// symmetric with `init`, which ends in standby too.
    pub fn transmit(&mut self, data: &[u8], timeout_ms: u32) -> Result<(), crate::sx1262::Error> {
        self.standby()?;
        self.packet_params(data.len() as u8)?;
        self.cmd(op::SET_DIO_IRQ_PARAMS, &sx126x::tx_irq_params())?;
        self.cmd(op::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
        block_on(self.bus.write_payload(data))?;
        self.cmd(op::SET_TX, &[0x00, 0x00, 0x00])?; // no hw timeout

        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(100) as u64);
        while !self.dio1.is_high() {
            if Instant::now() >= deadline {
                let _ = self.standby();
                return Err(crate::sx1262::Error::Busy);
            }
        }
        let mut irq = [0u8; 2];
        self.read(op::GET_IRQ_STATUS, &mut irq)?;
        self.cmd(op::CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
        if u16::from_be_bytes(irq) & sx126x::IRQ_TX_DONE != 0 {
            Ok(())
        } else {
            let _ = self.standby();
            Err(crate::sx1262::Error::Busy)
        }
    }
}
