//! The BetterSync daemon as a library, so tests can run several in one
//! process. The `bsyncd` binary is a thin wrapper.

pub mod api;
pub mod config;
pub mod daemon;
mod ui;

pub use config::{Config, default_config_dir};
pub use daemon::{Daemon, DaemonStatus, FolderStatus, PeerStatus};
