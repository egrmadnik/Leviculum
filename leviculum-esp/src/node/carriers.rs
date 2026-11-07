//! The LoRa carrier: bring the SX1262 up, move frames in and out.
//!
//! One interface, `InterfaceId(0)`. A radio that fails to come up leaves
//! the board logging `[LORA] state=down` and running everything else — a
//! sensor node without a mesh is degraded, not dead.

use alloc::vec::Vec;

use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_hal::Async;
use leviculum_core::transport::{Action, TickOutput};
use leviculum_core::InterfaceId;

use crate::node::board::{Board, LoraProfile, LoraWiring};
use crate::node::boot::{self, Booted};
use crate::node::reticulum::Node;
use crate::radio::{Radio, Rx};
use crate::sx1262::Sx1262Bus;
use crate::UsbLog;

type LoraRadio = Radio<'static, Spi<'static, Async>>;

/// The radio, plus the receive and transmit state that goes with it.
pub struct Carrier {
    radio: Option<LoraRadio>,
    profile: LoraProfile,
    lora_buf: [u8; 256],
    reassembler: leviculum_core::rnode::SplitReassembler,
    rx_tick: u32,
    tx_seq: u8,
}

/// Open the SX1262's port and bring the radio up: init → configure →
/// continuous RX, the same order the nRF boards run on the same
/// `leviculum_core::sx126x` sequences.
pub fn start<B: Board>(boot: &mut Booted, w: LoraWiring) -> Carrier {
    // Chip-select high before the bus exists: a low line while the SoC is
    // still booting is the start of a command the chip will wait to
    // finish.
    let nss = Output::new(w.nss, Level::High, OutputConfig::default());
    let reset = Output::new(w.reset, Level::High, OutputConfig::default());
    // BUSY and DIO1 are outputs of the radio; no pull, the chip drives
    // them.
    let busy = Input::new(w.busy, InputConfig::default().with_pull(Pull::None));
    let dio1 = Input::new(w.dio1, InputConfig::default().with_pull(Pull::None));
    // Parked low until the radio's `listen` asserts it.
    let rxen = Output::new(w.rxen, Level::Low, OutputConfig::default());

    let spi = match Spi::new(
        w.spi,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(w.spi_freq_hz))
            // SX126x: CPOL=0, CPHA=0 (datasheet rev 2.1 §13.1).
            .with_mode(Mode::_0),
    ) {
        Ok(spi) => spi
            .with_sck(w.sck)
            .with_mosi(w.mosi)
            .with_miso(w.miso)
            .into_async(),
        Err(_) => {
            boot.log
                .line("[LORA] ", format_args!("state=down reason=spi-config"));
            boot::halt_blinking::<B>(boot);
        }
    };

    let p = B::LORA;
    let mut radio = Radio::new(Sx1262Bus::new(spi, nss, busy), reset, dio1, rxen);
    let up = match radio.init(p.freq_hz) {
        Ok(chip_status) => {
            match radio.configure(p.freq_hz, p.sf, p.bw_code, p.cr_code, p.power_dbm, p.preamble_symbols)
            {
                Ok(program) => {
                    let _ = radio.listen();
                    boot.log.line(
                        "[LORA] ",
                        format_args!(
                            "state=up chip=0x{:02X} freq={} sf={} bw=125k power={}dBm",
                            chip_status, p.freq_hz, p.sf, program.programmed_dbm
                        ),
                    );
                    true
                }
                Err(_) => {
                    boot.log
                        .line("[LORA] ", format_args!("state=down reason=configure"));
                    false
                }
            }
        }
        Err(_) => {
            boot.log
                .line("[LORA] ", format_args!("state=down reason=init"));
            false
        }
    };

    Carrier {
        radio: if up { Some(radio) } else { None },
        profile: p,
        lora_buf: [0u8; 256],
        reassembler: leviculum_core::rnode::SplitReassembler::new(),
        rx_tick: 0,
        tx_seq: 0,
    }
}

impl Carrier {
    /// Whether the radio came up. Without it the node has no interface.
    pub fn is_up(&self) -> bool {
        self.radio.is_some()
    }

    /// Poll the radio once. A complete Reticulum packet goes to the node
    /// and what comes back is returned for the caller to act on.
    pub fn ingest(&mut self, node: &mut Node, log: &mut UsbLog<'static>) -> Option<TickOutput> {
        let radio = self.radio.as_mut()?;
        match radio.poll_rx(&mut self.lora_buf) {
            Ok(Rx::Frame { len, status }) => {
                log.line(
                    "[LORA] ",
                    format_args!("rx len={} rssi={} snr={}", len, status.rssi, status.snr),
                );
                let out = self
                    .reassembler
                    .feed(&self.lora_buf[..len as usize], self.rx_tick)
                    .map(|packet| node.handle_packet(InterfaceId(0), &packet));
                self.rx_tick = self.rx_tick.wrapping_add(1);
                out
            }
            Ok(Rx::BadCrc) => {
                log.line("[LORA] ", format_args!("rx err=crc"));
                None
            }
            Ok(Rx::Idle) => None,
            Err(_) => {
                log.line("[LORA] ", format_args!("rx err=bus"));
                let _ = radio.listen();
                None
            }
        }
    }

    /// What a `TickOutput` becomes on this board: every `SendPacket` and
    /// `Broadcast` goes onto the air as RNode-framed LoRa (the 254-byte
    /// split a Reticulum packet is entitled to).
    ///
    /// After a transmit the chip is in standby — `listen` re-arms the
    /// continuous RX the loop expects.
    pub fn dispatch(&mut self, out: TickOutput, log: &mut UsbLog<'static>) {
        let Some(radio) = self.radio.as_mut() else {
            return;
        };
        for action in out.actions {
            let data: Vec<u8> = match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            for frame in leviculum_core::rnode::build_lora_frames(&data, self.tx_seq << 4) {
                let timeout = leviculum_core::sx126x::tx_timeout_ms(
                    frame.len() as u32,
                    self.profile.bw_hz,
                    self.profile.sf,
                    self.profile.cr_denominator,
                    self.profile.preamble_symbols,
                );
                match radio.transmit(&frame, timeout) {
                    Ok(()) => log.line("[LORA] ", format_args!("tx len={}", frame.len())),
                    Err(_) => log.line("[LORA] ", format_args!("tx err=timeout")),
                }
            }
            self.tx_seq = self.tx_seq.wrapping_add(1) & 0x0F;
            let _ = radio.listen();
        }
    }
}
