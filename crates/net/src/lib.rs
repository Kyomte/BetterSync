//! Networking for BetterSync: the [`Transport`] trait the engine sends
//! through, device identity, pinned-certificate TLS and the QUIC transport.

use std::io;

use bsync_core::DeviceId;

pub mod dialer;
pub mod discovery;
mod identity;
pub mod metered;
pub mod quic;
pub mod rate;
pub mod tailscale;
pub mod tls;

pub use dialer::Dialer;
pub use discovery::Discovery;
pub use identity::Identity;
pub use quic::{EventSink, FoldersFor, NetEvent, QuicNet, QuicTransport};
pub use rate::RateLimit;
pub use tls::PairedSet;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("peer {0:?} is unreachable")]
    Unreachable(DeviceId),
    #[error("device {0} is not paired")]
    NotPaired(DeviceId),
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl NetError {
    pub(crate) fn tls(e: impl std::fmt::Display) -> Self {
        Self::Tls(e.to_string())
    }
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
