//! The one wire shape every device payload uses: a 4-byte magic, a
//! version byte, the payload, a CRC32 over all of it.
//!
//! ```text
//! +--------+-----+------------------+-------+
//! | magic  | ver | payload (n bytes)| crc32 |
//! | 4 B    | 1 B | n B              | 4 B LE|
//! +--------+-----+------------------+-------+
//! ```
//!
//! The payload layout is the device's (`Params::write`/`Params::read`);
//! this module owns only the envelope, so every device gets the same
//! three rejections — wrong magic, wrong version, bad CRC — and the
//! same test proves them.

use alloc::vec::Vec;

/// Why a frame was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than magic + version + CRC.
    TooShort,
    /// Not this frame type.
    Magic,
    /// A layout this build does not read.
    Version,
    /// Bytes changed between encode and decode.
    Crc,
    /// The payload decoded but the device refused the values.
    Invalid,
}

/// Bytes of envelope around the payload.
pub const OVERHEAD: usize = 4 + 1 + 4;

/// Wrap `payload` in the envelope.
pub fn encode(magic: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(OVERHEAD + payload.len());
    out.extend_from_slice(magic);
    out.push(version);
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Strip and check the envelope; the slice returned is the payload.
pub fn decode<'a>(bytes: &'a [u8], magic: &[u8; 4], version: u8) -> Result<&'a [u8], FrameError> {
    if bytes.len() < OVERHEAD {
        return Err(FrameError::TooShort);
    }
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    if crc32(body) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
        return Err(FrameError::Crc);
    }
    if &body[..4] != magic {
        return Err(FrameError::Magic);
    }
    if body[4] != version {
        return Err(FrameError::Version);
    }
    Ok(&body[5..])
}

/// CRC-32 (IEEE, reflected 0xEDB88320), table-less.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Little-endian field cursor — the device's `read` uses it so a layout
/// is a sequence of `take_f32()`/`take_u32()` and not index arithmetic.
pub struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], FrameError> {
        if self.bytes.len() < N {
            return Err(FrameError::TooShort);
        }
        let (h, t) = self.bytes.split_at(N);
        self.bytes = t;
        Ok(h.try_into().unwrap())
    }

    pub fn f32(&mut self) -> Result<f32, FrameError> {
        self.take::<4>().map(f32::from_le_bytes)
    }

    pub fn u32(&mut self) -> Result<u32, FrameError> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    pub fn u16(&mut self) -> Result<u16, FrameError> {
        self.take::<2>().map(u16::from_le_bytes)
    }

    pub fn u8(&mut self) -> Result<u8, FrameError> {
        self.take::<1>().map(|b| b[0])
    }

    pub fn i16(&mut self) -> Result<i16, FrameError> {
        self.take::<2>().map(i16::from_le_bytes)
    }

    /// Bytes not yet consumed — a decoder that wants to be strict
    /// checks this is empty.
    pub fn remaining(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &[u8; 4] = b"TEST";

    #[test]
    fn roundtrip() {
        let f = encode(M, 1, &[1, 2, 3]);
        assert_eq!(f.len(), OVERHEAD + 3);
        assert_eq!(decode(&f, M, 1).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn rejections() {
        let f = encode(M, 1, &[9]);
        assert_eq!(decode(&f[..5], M, 1), Err(FrameError::TooShort));
        assert_eq!(decode(&f, b"NOPE", 1), Err(FrameError::Magic));
        assert_eq!(decode(&f, M, 2), Err(FrameError::Version));
        let mut bad = f.clone();
        bad[5] ^= 0xFF;
        assert_eq!(decode(&bad, M, 1), Err(FrameError::Crc));
    }

    #[test]
    fn cursor_reads_in_order() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1.5f32.to_le_bytes());
        buf.extend_from_slice(&7u32.to_le_bytes());
        let mut c = Cursor::new(&buf);
        assert_eq!(c.f32().unwrap(), 1.5);
        assert_eq!(c.u32().unwrap(), 7);
        assert_eq!(c.remaining(), 0);
        assert_eq!(c.u8(), Err(FrameError::TooShort));
    }
}
