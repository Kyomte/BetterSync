use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Identity of a device: its Ed25519 public key.
///
/// Peers pin each other's `DeviceId` at pairing time; only paired IDs are
/// ever accepted on a connection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// First 7 base32 characters — enough to tell devices apart in file
    /// names such as conflict copies.
    pub fn short(&self) -> String {
        let full = self.to_string();
        full[..7].to_owned()
    }
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl fmt::Display for DeviceId {
    /// RFC 4648 base32 without padding, grouped in fours: `ABCD-EFGH-…`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut chars = Vec::with_capacity(52);
        let mut buffer: u32 = 0;
        let mut bits = 0;
        for &b in &self.0 {
            buffer = (buffer << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                chars.push(BASE32[((buffer >> bits) & 31) as usize]);
            }
        }
        if bits > 0 {
            chars.push(BASE32[((buffer << (5 - bits)) & 31) as usize]);
        }
        for (i, group) in chars.chunks(4).enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            f.write_str(std::str::from_utf8(group).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a valid device ID (expected 52 base32 characters, dashes optional)")]
pub struct ParseDeviceIdError;

impl FromStr for DeviceId {
    type Err = ParseDeviceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; 32];
        let mut n = 0;
        let mut buffer: u32 = 0;
        let mut bits = 0;
        for c in s.bytes().filter(|&c| c != b'-' && c != b' ') {
            let c = c.to_ascii_uppercase();
            let v = BASE32
                .iter()
                .position(|&b| b == c)
                .ok_or(ParseDeviceIdError)? as u32;
            buffer = (buffer << 5) | v;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                *out.get_mut(n).ok_or(ParseDeviceIdError)? = (buffer >> bits) as u8;
                n += 1;
            }
            buffer &= (1 << bits) - 1;
        }
        // 52 chars = 260 bits = 32 bytes + 4 zero padding bits.
        if n != 32 || bits != 4 || buffer != 0 {
            return Err(ParseDeviceIdError);
        }
        Ok(Self(out))
    }
}

/// Identifies a synced folder; shared by every device that syncs it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FolderId(pub String);

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_parse_round_trip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let id = DeviceId::from_bytes(bytes);
        let text = id.to_string();
        assert_eq!(text.len(), 52 + 12);
        assert_eq!(text.parse::<DeviceId>().unwrap(), id);
        assert_eq!(
            text.to_lowercase()
                .replace('-', "")
                .parse::<DeviceId>()
                .unwrap(),
            id
        );
        assert_eq!(id.short(), text[..7]);
        assert!("ABCD".parse::<DeviceId>().is_err());
        assert!(format!("{text}A").parse::<DeviceId>().is_err());
        assert!("1".repeat(52).parse::<DeviceId>().is_err());
    }

    #[test]
    fn ordering_follows_bytes() {
        assert!(DeviceId::from_bytes([1; 32]) < DeviceId::from_bytes([2; 32]));
    }
}
