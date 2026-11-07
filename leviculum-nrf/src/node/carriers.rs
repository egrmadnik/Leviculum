//! The two carriers an LNode routes over besides USB: the SX1262 LoRa
//! radio and the BLE peripheral.

use embassy_executor::Spawner;
use leviculum_core::envelope::MediaProfileWire;

use crate::ble::BleChannels;
use crate::lora::LoRaChannels;
use crate::node::board::{Board, BlePeripherals, LoraWiring};
use crate::{info, log_critical};

/// Bring up the SX1262 and, when the media profile allows it, spawn the
/// task that drives it. Returns the channels the main loop reads and
/// writes.
pub async fn start_lora<B: Board>(
    spawner: &Spawner,
    spi2: embassy_nrf::Peri<'static, embassy_nrf::peripherals::SPI2>,
    wiring: LoraWiring,
    media: &MediaProfileWire,
) -> LoRaChannels {
    // LoRa on SPIM2, the instance the shared `lora::init` signature
    // dictates. The pin map comes from the board's `Board::wire`.
    log_critical!("[STG] lora-init");
    let lora = crate::lora::init(
        spi2,
        wiring.sck,
        wiring.mosi,
        wiring.miso,
        wiring.cs,
        wiring.reset,
        wiring.busy,
        wiring.dio1,
        wiring.rx_enable,
        wiring.spi_freq,
        B::CONFIG.lora_tcxo_voltage_reg,
    )
    .await;
    info!("SX1262 ready");

    // Radio profile: whatever a host last set and we persisted, else the
    // compiled default. A blank or corrupt page decodes to None.
    let radio_cfg = match crate::radio_store::load(B::CONFIG.radio_config_flash_page)
        .and_then(crate::lora::RadioConfig::from_wire_config)
    {
        Some(cfg) => {
            crate::log::log_fmt(
                "[RADIO] ",
                format_args!(
                    "persisted freq={} bw={} sf={} cr={} pwr={}",
                    cfg.frequency_hz, cfg.bw_hz, cfg.sf, cfg.cr_denom, cfg.tx_power_dbm
                ),
            );
            cfg
        }
        None => {
            crate::log::log_fmt("[RADIO] ", format_args!("default eu_medium"));
            crate::lora::RadioConfig::eu_medium()
        }
    };
    let lora_channels = crate::lora::channels();
    // `lora=off` means the radio stays down: the task that resets,
    // configures and keys the SX1262 is never spawned, so the chip is
    // never brought out of the state `lora::init` left it in. Nothing
    // transmits and nothing is received — which is the point, since a
    // reception over the medium under test's neighbour is exactly what
    // makes a single-medium measurement falsifiable.
    if media.lora_enabled {
        // Per-board entropy for the TX channel-access randomness: a fixed
        // seed here would make co-booted boards draw identical pre-TX
        // jitter and collide anyway (see `lora_task`).
        let channel_seed = crate::lora::channel_seed();
        spawner.must_spawn(crate::lora::lora_task(
            lora,
            radio_cfg,
            channel_seed,
        ));
        crate::boot_trace::phase(crate::boot_trace::Phase::LoraTask);
    } else {
        crate::media::log_carrier_held_down("lora");
        crate::boot_trace::phase(crate::boot_trace::Phase::LoraSkipped);
    }

    lora_channels
}

/// Enable the SoftDevice and start the BLE peripheral. Returns the
/// SoftDevice handle (the flash stores and the telemetry die-temperature
/// read need it) and the BLE channels.
pub fn start_ble(
    spawner: &Spawner,
    ble: BlePeripherals,
    media: &MediaProfileWire,
    media_src: crate::media::Source,
    identity_hash: [u8; 16],
    vbus: &'static crate::SoftwareVbusDetect,
) -> (&'static nrf_softdevice::Softdevice, BleChannels) {
    // BLE — full init restored. RAM ORIGIN bumped to 40K (memory.x) to give
    // Softdevice::enable headroom for our config (att_mtu=256, …).
    // Freeze the BLE name for this boot, before the SoftDevice is
    // enabled and before the advertisement is built: both read it, and a
    // name that changed between them would put two different strings on
    // the two BLE surfaces. This also publishes the identity hash the
    // control-envelope report derives its defaults from.
    crate::name::note_boot_name(&identity_hash);
    log_critical!("[STG] ble-init");
    let sd = crate::ble::init(
        spawner,
        media.ble_enabled,
        identity_hash,
        vbus,
        ble.rtc0,
        ble.timer0,
        ble.temp,
        ble.ppi_ch19,
        ble.ppi_ch30,
        ble.ppi_ch31,
        ble.ppi_ch17,
        ble.ppi_ch18,
        ble.ppi_ch20,
        ble.ppi_ch21,
        ble.ppi_ch22,
        ble.ppi_ch23,
        ble.ppi_ch24,
        ble.ppi_ch25,
        ble.ppi_ch26,
        ble.ppi_ch27,
        ble.ppi_ch28,
        ble.ppi_ch29,
        ble.rng,
    );
    let ble_channels = crate::ble::channels();
    info!("BLE ready");

    // Both spawn decisions are made: say what actually came up, then
    // prove it on the debug port. `note_boot_state` is passed what was
    // really spawned, which is what makes "switching a medium on takes
    // effect at reboot" a fact the board can state rather than a hope.
    crate::media::note_boot_state(media.lora_enabled, media.ble_enabled);
    crate::media::log_banner(media_src);
    crate::name::log_banner();

    (sd, ble_channels)
}
