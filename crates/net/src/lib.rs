//! Networking for BetterSync: the [`Transport`] trait, and (from M4) the QUIC
//! transport, device identity, pairing and discovery.
//!
//! The engine only talks to peers through [`Transport`] so the simulator can
//! swap in an in-memory network. The trait's shape is provisional and is
//! expected to be revised when the QUIC transport lands in M4.

use bsync_core::DeviceId;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("peer {0:?} is unreachable")]
    Unreachable(DeviceId),
}

/// A device's outgoing connection to its peers. Messages are opaque byte
/// strings; encoding them is `bsync-proto`'s job. Incoming messages are
/// handed to the engine by whoever drives it (the daemon or the simulator).
pub trait Transport: Send + Sync {
    /// The device this endpoint belongs to.
    fn local_id(&self) -> DeviceId;

    /// Queue `msg` for delivery to `to`. Delivery is not guaranteed: the
    /// connection may drop before the message arrives.
    fn send(&self, to: DeviceId, msg: Vec<u8>) -> Result<(), NetError>;
}
