//! Per-folder sync engine: glues index, fs, proto and net. Runs in-process
//! with an injected file system and transport so the simulator can drive it.

mod error;
pub mod folder;
pub mod scan;
pub mod sync;

pub use error::{EngineError, Result};
pub use folder::{ChunkSource, Clock, Folder, NoChunks, Outcome, SyncStats, SystemClock};
pub use sync::{EngineConfig, EngineStatus, SyncEngine};
