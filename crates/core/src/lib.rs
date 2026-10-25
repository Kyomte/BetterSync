//! Pure sync logic for BetterSync: file metadata model, version vectors,
//! chunk lists and the sync planner.
//!
//! This crate does no I/O and is not async, so it can be property-tested and
//! driven by the deterministic simulator.

use std::fmt;

/// Identity of a device: its Ed25519 public key.
///
/// Peers pin each other's `DeviceId` at pairing time; only paired IDs are
/// ever accepted on a connection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId(")?;
        for b in &self.0[..4] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_round_trips_and_orders_by_bytes() {
        let a = DeviceId::from_bytes([1; 32]);
        let b = DeviceId::from_bytes([2; 32]);
        assert_eq!(a.as_bytes(), &[1; 32]);
        assert!(a < b);
        assert_eq!(format!("{a:?}"), "DeviceId(01010101…)");
    }
}
