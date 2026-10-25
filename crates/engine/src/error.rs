use std::io;

use bsync_index::IndexError;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Index(#[from] IndexError),
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;
