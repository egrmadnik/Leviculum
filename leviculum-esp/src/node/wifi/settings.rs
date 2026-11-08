//! Runtime WiFi settings: which network the radio joins and how the UDP
//! and TCP sockets are wired — changeable on the debug port, persisted
//! to flash, applied live. No rebuild for a new lab.
//!
//! This module is deliberately free of firmware types (no esp-hal, no
//! embassy, no flash driver): the parse, validation and flash-frame code
//! below is the same file a host `rustc --test` compiles, so the
//! command grammar is exercised off-target. The shared mutable state and
//! the task plumbing live in `super` (`wifi/mod.rs`), which is the part
//! only the firmware can test.
//!
//! The compile-time env knobs are not gone — they are the *defaults* a
//! fresh flash starts from:
//!
//! | env (at build) | meaning | default |
//! |---|---|---|
//! | `LEVICULUM_WIFI_SSID` | network to join | unset → WiFi dark |
//! | `LEVICULUM_WIFI_PSK` | WPA2 password | empty → open network |
//! | `LEVICULUM_UDP_PORT` | UDP listen and forward port | `4968` |
//! | `LEVICULUM_UDP_FORWARD` | IPv4 datagrams go to | `255.255.255.255` |
//! | `LEVICULUM_TCP_TARGET` | `a.b.c.d:port` to dial | unset → no task |
//!
//! A `WIFI` command on the debug port overrides the running value, and
//! `WIFI SAVE` writes the result to flash so the next boot starts from
//! the stored settings instead of the compiled defaults.

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

extern crate alloc;

/// Longest SSID bytes the radio accepts.
pub const MAX_SSID: usize = 32;
/// Longest WPA2 passphrase bytes the radio accepts.
pub const MAX_PSK: usize = 64;
/// The UDP port both directions, unless the env said otherwise.
pub const UDP_PORT_DEFAULT: u16 = 4968;
/// Datagrams go to the LAN broadcast address unless told otherwise.
pub const FORWARD_DEFAULT: [u8; 4] = [255, 255, 255, 255];

/// Everything the WiFi carrier runs on. `None`/empty mean "off".
#[derive(Clone, Debug, PartialEq)]
pub struct WifiSettings {
    /// The network to join; `None` keeps the radio dark.
    pub ssid: Option<String>,
    /// WPA2 passphrase; empty means an open network.
    pub psk: String,
    /// The UDP interface's listen and forward port.
    pub udp_port: u16,
    /// Where UDP datagrams go; `None` makes the interface receive-only.
    pub udp_forward: Option<[u8; 4]>,
    /// The TCP client's `a.b.c.d:port`; `None` disables the interface.
    pub tcp_target: Option<([u8; 4], u16)>,
}

impl WifiSettings {
    /// The build-time env knobs, as a starting point. A flashed board
    /// whose flash slot is blank (or whose slot decodes to nothing)
    /// runs on exactly what the compiled defaults say.
    pub fn env_defaults() -> Self {
        let forward = match option_env!("LEVICULUM_UDP_FORWARD") {
            Some(s) => parse_ipv4(s).unwrap_or(FORWARD_DEFAULT),
            None => FORWARD_DEFAULT,
        };
        Self {
            ssid: option_env!("LEVICULUM_WIFI_SSID").map(String::from),
            psk: option_env!("LEVICULUM_WIFI_PSK").unwrap_or("").to_string(),
            udp_port: parse_port(option_env!("LEVICULUM_UDP_PORT")).unwrap_or(UDP_PORT_DEFAULT),
            udp_forward: Some(forward),
            tcp_target: option_env!("LEVICULUM_TCP_TARGET").and_then(parse_endpoint),
        }
    }
}

/// `a.b.c.d` → IPv4 octets, or `None`. Runtime parse — the command text
/// and the env values are strings, the sockets want numbers.
pub fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut i = 0;
    for part in text.split('.') {
        if i == 4 {
            return None;
        }
        out[i] = part.parse().ok()?;
        i += 1;
    }
    (i == 4).then_some(out)
}

/// `a.b.c.d:port` → an endpoint, or `None`.
pub fn parse_endpoint(text: &str) -> Option<([u8; 4], u16)> {
    let (addr, port) = text.split_once(':')?;
    Some((parse_ipv4(addr)?, port.parse().ok()?))
}

/// Decimal text → a port number, or `None` when it is not 0..=65535.
pub fn parse_port(text: Option<&str>) -> Option<u16> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for b in text.bytes() {
        if !(b.is_ascii_digit()) {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
        if v > u16::MAX as u32 {
            return None;
        }
    }
    Some(v as u16)
}

/// A debug-port `WIFI` command, parsed but not yet applied.
#[derive(Clone, Debug, PartialEq)]
pub enum WifiCmd {
    /// `WIFI STATUS` — print the running settings.
    Status,
    /// `WIFI SSID <name|off>` — the network to join (`off` = dark).
    SetSsid(Option<String>),
    /// `WIFI PSK <pass|none>` — the WPA2 passphrase (`none` = open).
    SetPsk(String),
    /// `WIFI UDP <port> [<a.b.c.d>|off]` — port and forward address.
    SetUdp { port: u16, forward: Option<[u8; 4]> },
    /// `WIFI TCP <a.b.c.d:port|off>` — the client's target.
    SetTcp(Option<([u8; 4], u16)>),
    /// `WIFI SAVE` — persist the running settings to flash.
    Save,
    /// `WIFI RESET` — back to the compiled defaults.
    Reset,
}

/// Parse one debug-port line.
///
/// `None` = not a `WIFI` command (the line belongs to the app).
/// `Some(Err(why))` = a `WIFI` command that does not parse; the reason
/// is short enough to log as-is.
pub fn parse_command(line: &str) -> Option<Result<WifiCmd, &'static str>> {
    let mut words = line.split_whitespace();
    let first = words.next()?;
    if !first.eq_ignore_ascii_case("wifi") {
        return None;
    }
    let verb = words.next().unwrap_or("");
    let mut rest = words;
    let result = match verb.to_ascii_uppercase().as_str() {
        "STATUS" => {
            if let Some(err) = extra(&mut rest, "too many words: WIFI STATUS") {
                return Some(Err(err));
            }
            Ok(WifiCmd::Status)
        }
        "SSID" => match rest.next() {
            None => Err("expected: WIFI SSID <name|off>"),
            Some(v) if v.eq_ignore_ascii_case("off") => {
                if let Some(err) = extra(&mut rest, "too many words: WIFI SSID off") {
                    return Some(Err(err));
                }
                Ok(WifiCmd::SetSsid(None))
            }
            Some(v) if v.len() > MAX_SSID => Err("SSID too long (max 32 chars)"),
            Some(v) => {
                if let Some(err) = extra(&mut rest, "too many words: WIFI SSID <name>") {
                    return Some(Err(err));
                }
                Ok(WifiCmd::SetSsid(Some(v.to_string())))
            }
        },
        "PSK" => match rest.next() {
            None => Err("expected: WIFI PSK <pass|none>"),
            Some(v) if v.eq_ignore_ascii_case("none") => {
                if let Some(err) = extra(&mut rest, "too many words: WIFI PSK none") {
                    return Some(Err(err));
                }
                Ok(WifiCmd::SetPsk(String::new()))
            }
            Some(v) if v.len() > MAX_PSK => Err("passphrase too long (max 64 chars)"),
            Some(v) => {
                if let Some(err) = extra(&mut rest, "too many words: WIFI PSK <pass>") {
                    return Some(Err(err));
                }
                Ok(WifiCmd::SetPsk(v.to_string()))
            }
        },
        "UDP" => {
            let port = match rest.next() {
                None => return Some(Err("expected: WIFI UDP <port> [<a.b.c.d>|off]")),
                Some(p) => match parse_port(Some(p)) {
                    None => return Some(Err("port must be 0-65535")),
                    Some(port) => port,
                },
            };
            let forward = match rest.next() {
                None => Some(FORWARD_DEFAULT),
                Some(v) if v.eq_ignore_ascii_case("off") => None,
                Some(v) => match parse_ipv4(v) {
                    Some(a) => Some(a),
                    None => return Some(Err("forward must be a.b.c.d or off")),
                },
            };
            if let Some(err) = extra(&mut rest, "too many words: WIFI UDP <port> [<forward>|off]") {
                return Some(Err(err));
            }
            Ok(WifiCmd::SetUdp { port, forward })
        }
        "TCP" => match rest.next() {
            None => Err("expected: WIFI TCP <a.b.c.d:port|off>"),
            Some(v) if v.eq_ignore_ascii_case("off") => {
                if let Some(err) = extra(&mut rest, "too many words: WIFI TCP off") {
                    return Some(Err(err));
                }
                Ok(WifiCmd::SetTcp(None))
            }
            Some(v) => match parse_endpoint(v) {
                None => Err("target must be a.b.c.d:port or off"),
                Some(ep) => {
                    if let Some(err) = extra(&mut rest, "too many words: WIFI TCP <target>") {
                        return Some(Err(err));
                    }
                    Ok(WifiCmd::SetTcp(Some(ep)))
                }
            },
        },
        "SAVE" => {
            if let Some(err) = extra(&mut rest, "too many words: WIFI SAVE") {
                return Some(Err(err));
            }
            Ok(WifiCmd::Save)
        }
        "RESET" => {
            if let Some(err) = extra(&mut rest, "too many words: WIFI RESET") {
                return Some(Err(err));
            }
            Ok(WifiCmd::Reset)
        }
        _ => Err("unknown: WIFI <STATUS|SSID|PSK|UDP|TCP|SAVE|RESET>"),
    };
    Some(result)
}

/// `None` = the command's token list is exhausted; `Some(cmd)` = a stray
/// token follows, and `cmd` is the "too many words" message.
fn extra(rest: &mut core::str::SplitWhitespace<'_>, cmd: &'static str) -> Option<&'static str> {
    if rest.next().is_some() {
        Some(cmd)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Flash frame: the settings as one contiguous record, CRC-guarded like
// the identity and blink slots. `FRAME_MAX` bounds the read buffer.
// ---------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"WFI2";
const FORWARD_FLAG: u8 = 0x55;
const TCP_FLAG: u8 = 0x66;

/// Upper bound on one encoded record: magic + all max-size fields + CRC.
pub const FRAME_MAX: usize = 4 + 1 + MAX_SSID + 1 + MAX_PSK + 2 + 1 + 4 + 1 + 6 + 4;

/// IEEE CRC-32, table-less. The identity slot uses the same polynomial
/// (via `pid::crc32_pub`); the table-less form keeps this module free of
/// crate imports so the host test compiles it standalone.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
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

/// Encode the settings into the flash record.
pub fn frame_encode(s: &WifiSettings) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_MAX);
    out.extend_from_slice(MAGIC);
    let ssid = s.ssid.as_deref().unwrap_or("");
    out.push(ssid.len() as u8);
    out.extend_from_slice(ssid.as_bytes());
    out.push(s.psk.len() as u8);
    out.extend_from_slice(s.psk.as_bytes());
    out.extend_from_slice(&s.udp_port.to_le_bytes());
    match s.udp_forward {
        Some(a) => {
            out.push(FORWARD_FLAG);
            out.extend_from_slice(&a);
        }
        None => out.push(0),
    }
    match s.tcp_target {
        Some((a, p)) => {
            out.push(TCP_FLAG);
            out.extend_from_slice(&a);
            out.extend_from_slice(&p.to_le_bytes());
        }
        None => out.push(0),
    }
    let crc = crc32(&out[MAGIC.len()..]);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Decode a flash record. `None` on any inconsistency — magic, length
/// bounds, CRC — which all mean "ignore it, use the defaults".
pub fn frame_decode(buf: &[u8]) -> Option<WifiSettings> {
    if buf.len() < MAGIC.len() || &buf[..4] != MAGIC {
        return None;
    }
    let mut i = 4;
    let take = |i: &mut usize, n: usize| -> Option<&[u8]> {
        let end = i.checked_add(n)?;
        let slice = buf.get(*i..end)?;
        *i = end;
        Some(slice)
    };
    let ssid_len = take(&mut i, 1)?[0] as usize;
    if ssid_len > MAX_SSID {
        return None;
    }
    let ssid = core::str::from_utf8(take(&mut i, ssid_len)?).ok()?;
    let psk_len = take(&mut i, 1)?[0] as usize;
    if psk_len > MAX_PSK {
        return None;
    }
    let psk = core::str::from_utf8(take(&mut i, psk_len)?).ok()?;
    let port = u16::from_le_bytes(take(&mut i, 2)?.try_into().ok()?);
    let forward = match take(&mut i, 1)?[0] {
        FORWARD_FLAG => Some(take(&mut i, 4)?.try_into().ok()?),
        _ => None,
    };
    let tcp_target = match take(&mut i, 1)?[0] {
        TCP_FLAG => {
            let addr: [u8; 4] = take(&mut i, 4)?.try_into().ok()?;
            let port = u16::from_le_bytes(take(&mut i, 2)?.try_into().ok()?);
            Some((addr, port))
        }
        _ => None,
    };
    let crc = u32::from_le_bytes(take(&mut i, 4)?.try_into().ok()?);
    if crc != crc32(&buf[4..i - 4]) {
        return None;
    }
    Some(WifiSettings {
        ssid: (!ssid.is_empty()).then(|| ssid.to_string()),
        psk: psk.to_string(),
        udp_port: port,
        udp_forward: forward,
        tcp_target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ipv4_roundtrip() {
        assert_eq!(parse_ipv4("192.168.1.10"), Some([192, 168, 1, 10]));
        assert_eq!(parse_ipv4("255.255.255.255"), Some([255, 255, 255, 255]));
        assert_eq!(parse_ipv4(""), None);
        assert_eq!(parse_ipv4("1.2.3"), None);
        assert_eq!(parse_ipv4("1.2.3.4.5"), None);
        assert_eq!(parse_ipv4("1.2.3.abc"), None);
        assert_eq!(parse_ipv4("256.1.1.1"), None);
    }

    #[test]
    fn parse_endpoint_and_port() {
        assert_eq!(parse_endpoint("10.0.0.2:4242"), Some(([10, 0, 0, 2], 4242)));
        assert_eq!(parse_endpoint("10.0.0.2:0"), Some(([10, 0, 0, 2], 0)));
        assert_eq!(parse_endpoint("10.0.0.2"), None);
        assert_eq!(parse_endpoint("10.0.0.2:99999"), None);
        assert_eq!(parse_endpoint("10.0.0.2:port"), None);
        assert_eq!(parse_port(Some("4968")), Some(4968));
        assert_eq!(parse_port(Some("65535")), Some(65535));
        assert_eq!(parse_port(Some("65536")), None);
        assert_eq!(parse_port(Some("")), None);
        assert_eq!(parse_port(Some("12a")), None);
        assert_eq!(parse_port(None), None);
    }

    #[test]
    fn not_a_wifi_line_is_none() {
        assert_eq!(parse_command("PID GET"), None);
        assert_eq!(parse_command("blink 1"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn wifi_command_parsing() {
        assert_eq!(
            parse_command("WIFI SSID lab"),
            Some(Ok(WifiCmd::SetSsid(Some("lab".to_string()))))
        );
        assert_eq!(
            parse_command("wifi ssid OFF"),
            Some(Ok(WifiCmd::SetSsid(None)))
        );
        assert_eq!(
            parse_command("WIFI PSK s3cret"),
            Some(Ok(WifiCmd::SetPsk("s3cret".to_string())))
        );
        assert_eq!(
            parse_command("WIFI PSK none"),
            Some(Ok(WifiCmd::SetPsk(String::new())))
        );
        assert_eq!(
            parse_command("WIFI UDP 4968"),
            Some(Ok(WifiCmd::SetUdp {
                port: 4968,
                forward: Some(FORWARD_DEFAULT)
            }))
        );
        assert_eq!(
            parse_command("WIFI UDP 5000 192.168.1.5"),
            Some(Ok(WifiCmd::SetUdp {
                port: 5000,
                forward: Some([192, 168, 1, 5])
            }))
        );
        assert_eq!(
            parse_command("WIFI UDP 5000 off"),
            Some(Ok(WifiCmd::SetUdp {
                port: 5000,
                forward: None
            }))
        );
        assert_eq!(
            parse_command("WIFI TCP 194.249.0.75:4242"),
            Some(Ok(WifiCmd::SetTcp(Some(([194, 249, 0, 75], 4242)))))
        );
        // Hostnames are not accepted — numeric IPv4 only.
        assert!(parse_command("WIFI TCP lab.nlzoh.si:4242")
            .unwrap()
            .is_err());
        assert_eq!(
            parse_command("WIFI TCP off"),
            Some(Ok(WifiCmd::SetTcp(None)))
        );
        assert_eq!(parse_command("WIFI STATUS"), Some(Ok(WifiCmd::Status)));
        assert_eq!(parse_command("WIFI SAVE"), Some(Ok(WifiCmd::Save)));
        assert_eq!(parse_command("WIFI RESET"), Some(Ok(WifiCmd::Reset)));
    }

    #[test]
    fn wifi_command_errors() {
        assert!(parse_command("WIFI").is_some());
        assert!(parse_command("WIFI").unwrap().is_err());
        assert!(parse_command("WIFI SSID").unwrap().is_err());
        // 33 chars is one over the 32-byte SSID bound.
        assert!(parse_command("WIFI SSID 123456789012345678901234567890123")
            .unwrap()
            .is_err());
        assert!(parse_command("WIFI UDP 70000").unwrap().is_err());
        assert!(parse_command("WIFI UDP 4968 1.2.3.999").unwrap().is_err());
        assert!(parse_command("WIFI FROB").unwrap().is_err());
    }

    #[test]
    fn frame_roundtrip() {
        let s = WifiSettings {
            ssid: Some("lab".to_string()),
            psk: "hunter2".to_string(),
            udp_port: 5000,
            udp_forward: Some([192, 168, 1, 5]),
            tcp_target: Some(([10, 0, 0, 2], 4242)),
        };
        let encoded = frame_encode(&s);
        assert!(encoded.len() <= FRAME_MAX);
        assert_eq!(frame_decode(&encoded), Some(s));
    }

    #[test]
    fn frame_roundtrip_off_state() {
        let s = WifiSettings {
            ssid: None,
            psk: String::new(),
            udp_port: 4968,
            udp_forward: None,
            tcp_target: None,
        };
        let encoded = frame_encode(&s);
        assert_eq!(frame_decode(&encoded), Some(s));
    }

    #[test]
    fn frame_decode_rejects_corruption() {
        let s = WifiSettings {
            ssid: Some("lab".to_string()),
            psk: String::new(),
            udp_port: 4968,
            udp_forward: Some(FORWARD_DEFAULT),
            tcp_target: None,
        };
        let mut encoded = frame_encode(&s);
        assert_eq!(frame_decode(&encoded), Some(s.clone()));
        // Flip one byte of the SSID: CRC must catch it.
        encoded[5] ^= 0xFF;
        assert_eq!(frame_decode(&encoded), None);
        // Wrong magic.
        let mut encoded = frame_encode(&s);
        encoded[0] = b'X';
        assert_eq!(frame_decode(&encoded), None);
        // Truncated.
        let encoded = frame_encode(&s);
        assert_eq!(frame_decode(&encoded[..encoded.len() - 1]), None);
        // Length over the bound.
        let mut encoded = frame_encode(&s);
        encoded[4] = MAX_SSID as u8 + 1;
        assert_eq!(frame_decode(&encoded), None);
    }

    #[test]
    fn crc32_is_stable() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
