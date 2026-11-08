//! One-sector flash persistence — the slots the PID node's durable
//! state lives in, so a remote-set tune and the node's own identity
//! survive reset and reflash.
//!
//! Region: two 4 KiB sectors at 7 MiB — `esp-storage` exposes no public
//! `capacity()`, so the offsets are fixed, and 7 MiB is chosen because
//! it lands inside even the smallest supported module (8 MB) while
//! staying far clear of the app image (~210 KB at 0x10000) and any
//! plausible partition-table change below it. A future data partition
//! is the case that revisits this.
//!
//! ```text
//! 0x700000  PID slot:  PIDC frame (34 B, self-CRC'd)
//!           +34        report-target destination hash (16 B)
//!           +50        flag 0x54 — target present
//! 0x701000  ID slot:   "IDT1" (4 B) + private key (64 B) + crc32 (4 B)
//! 0x702000  blink slot: "LBLK" (4 B) + frequency in hundredths of a hertz
//!           (u16 LE) + check word (the frequency XOR 0xB11C, u16 LE)
//! 0x703000  wifi slot: "WFI2" record — ssid, psk, UDP port/forward, TCP
//!           target, CRC-guarded (`node::wifi::settings`, `wifi` feature)
//! ```
//!
//! No wear levelling: a config write is a user event, dozens a year
//! against a 100 k-erase sector is nowhere near the limit; a looping
//! writer is a bug this doesn't hide.

use crate::pid::{crc32_pub as crc32, Config, CONFIG_FRAME_LEN};

/// Byte offset of the config slot — 7 MiB.
const SLOT: u32 = 0x0070_0000;
/// The identity sector right after it.
const ID_SLOT: u32 = 0x0070_1000;
/// Sector size the driver erases in.
const SECTOR: u32 = 4096;

/// The byte that says "the 16 target bytes after the frame are real".
const TARGET_PRESENT: u8 = 0x54;

const ID_MAGIC: &[u8; 4] = b"IDT1";
/// `private_key_bytes()` — x25519 + ed25519 private halves.
const ID_KEY_LEN: usize = 64;
const ID_FRAME_LEN: usize = 4 + ID_KEY_LEN + 4;

/// What the PID slot remembered, if it held a valid frame.
/// `None` = fresh flash or a corrupt frame — both mean "default".
pub fn load_config(
    store: &mut esp_storage::FlashStorage<'_>,
) -> Option<(Config, Option<[u8; 16]>)> {
    let mut buf = [0u8; CONFIG_FRAME_LEN + 17];
    if store.read(SLOT, &mut buf).is_err() {
        return None;
    }
    let cfg = Config::decode(&buf[..CONFIG_FRAME_LEN]).ok()?;
    let target = if buf[CONFIG_FRAME_LEN + 16] == TARGET_PRESENT {
        let mut t = [0u8; 16];
        t.copy_from_slice(&buf[CONFIG_FRAME_LEN..CONFIG_FRAME_LEN + 16]);
        Some(t)
    } else {
        None
    };
    Some((cfg, target))
}

/// Persist config + report target: erase the slot sector, write both.
/// `Err` is the raw flash error — the caller logs it.
pub fn save_config(
    store: &mut esp_storage::FlashStorage<'_>,
    cfg: &Config,
    target: Option<&[u8; 16]>,
) -> Result<(), esp_storage::FlashStorageError> {
    let mut buf = [0u8; CONFIG_FRAME_LEN + 17];
    buf[..CONFIG_FRAME_LEN].copy_from_slice(&cfg.encode());
    if let Some(t) = target {
        buf[CONFIG_FRAME_LEN..CONFIG_FRAME_LEN + 16].copy_from_slice(t);
        buf[CONFIG_FRAME_LEN + 16] = TARGET_PRESENT;
    }
    store.erase(SLOT, SLOT + SECTOR)?;
    store.write(SLOT, &buf)
}

/// The persisted identity's private key, or `None` if the slot is
/// fresh/corrupt — a corrupt identity slot is regenerated, not trusted.
pub fn load_identity(store: &mut esp_storage::FlashStorage<'_>) -> Option<[u8; ID_KEY_LEN]> {
    let mut buf = [0u8; ID_FRAME_LEN];
    if store.read(ID_SLOT, &mut buf).is_err() {
        return None;
    }
    if &buf[..4] != ID_MAGIC {
        return None;
    }
    if crc32(&buf[..4 + ID_KEY_LEN])
        != u32::from_le_bytes(buf[4 + ID_KEY_LEN..].try_into().unwrap())
    {
        return None;
    }
    Some(buf[4..4 + ID_KEY_LEN].try_into().unwrap())
}

/// Persist a freshly generated identity — once, ever. The first boot
/// writes it; every later boot reads the same one so the node's
/// destination hash is stable.
pub fn save_identity(
    store: &mut esp_storage::FlashStorage<'_>,
    key: &[u8; ID_KEY_LEN],
) -> Result<(), esp_storage::FlashStorageError> {
    let mut buf = [0u8; ID_FRAME_LEN];
    buf[..4].copy_from_slice(ID_MAGIC);
    buf[4..4 + ID_KEY_LEN].copy_from_slice(key);
    let c = crc32(&buf[..4 + ID_KEY_LEN]);
    buf[4 + ID_KEY_LEN..].copy_from_slice(&c.to_le_bytes());
    store.erase(ID_SLOT, ID_SLOT + SECTOR)?;
    store.write(ID_SLOT, &buf)
}

/// The Blink app's slot, the sector after the identity.
const BLINK_SLOT: u32 = 0x0070_2000;
const BLINK_MAGIC: &[u8; 4] = b"LBLK";
const BLINK_CHECK_MASK: u16 = 0xB11C;
const BLINK_FRAME_LEN: usize = 8;

/// The persisted blink frequency in hundredths of a hertz, or `None` if the
/// slot is blank (erased flash is all `0xFF`) or the check word does not
/// match — both mean "use the default".
pub fn load_blink(store: &mut esp_storage::FlashStorage<'_>) -> Option<u16> {
    let mut buf = [0u8; BLINK_FRAME_LEN];
    if store.read(BLINK_SLOT, &mut buf).is_err() || &buf[..4] != BLINK_MAGIC {
        return None;
    }
    let centi = u16::from_le_bytes([buf[4], buf[5]]);
    let check = u16::from_le_bytes([buf[6], buf[7]]);
    (check == centi ^ BLINK_CHECK_MASK).then_some(centi)
}

/// Persist the blink frequency: erase the sector, write the record.
pub fn save_blink(
    store: &mut esp_storage::FlashStorage<'_>,
    centi_hz: u16,
) -> Result<(), esp_storage::FlashStorageError> {
    let mut buf = [0u8; BLINK_FRAME_LEN];
    buf[..4].copy_from_slice(BLINK_MAGIC);
    buf[4..6].copy_from_slice(&centi_hz.to_le_bytes());
    buf[6..8].copy_from_slice(&(centi_hz ^ BLINK_CHECK_MASK).to_le_bytes());
    store.erase(BLINK_SLOT, BLINK_SLOT + SECTOR)?;
    store.write(BLINK_SLOT, &buf)
}

/// The WiFi settings slot, the sector after the blink slot.
#[cfg(feature = "wifi")]
const WIFI_SLOT: u32 = 0x0070_3000;

/// The persisted WiFi settings, or `None` when the slot is blank or its
/// record does not decode (magic, bounds, CRC) — both mean "use the
/// compiled defaults".
#[cfg(feature = "wifi")]
pub fn load_wifi_settings(
    store: &mut esp_storage::FlashStorage<'_>,
) -> Option<crate::node::wifi::settings::WifiSettings> {
    use crate::node::wifi::settings;
    let mut buf = [0u8; settings::FRAME_MAX];
    if store.read(WIFI_SLOT, &mut buf).is_err() {
        return None;
    }
    settings::frame_decode(&buf)
}

/// Persist the WiFi settings: erase the sector, write the record.
#[cfg(feature = "wifi")]
pub fn save_wifi_settings(
    store: &mut esp_storage::FlashStorage<'_>,
    s: &crate::node::wifi::settings::WifiSettings,
) -> Result<(), esp_storage::FlashStorageError> {
    use crate::node::wifi::settings;
    let frame = settings::frame_encode(s);
    store.erase(WIFI_SLOT, WIFI_SLOT + SECTOR)?;
    store.write(WIFI_SLOT, &frame)
}
