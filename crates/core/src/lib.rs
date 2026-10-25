//! Pure sync logic for BetterSync: file metadata model, version vectors,
//! chunk lists and the sync planner.
//!
//! This crate does no I/O and is not async, so it can be property-tested and
//! driven by the deterministic simulator. (The chunker reads from a generic
//! `std::io::Read`, which the caller supplies.)

pub mod chunk;
mod hash;
mod id;
pub mod plan;
mod record;
mod time;
mod vv;

pub use chunk::{ChunkParams, ChunkedFile, chunk_bytes, chunk_reader};
pub use hash::Hash;
pub use id::{DeviceId, FolderId};
pub use record::{ChunkRef, EntryKind, FileRecord, is_valid_rel_path, parent_of};
pub use time::format_utc_compact;
pub use vv::{Causality, VersionVector};
