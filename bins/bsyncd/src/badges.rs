//! Explorer sync badges (stretch goal S1, Windows): keeps each file's
//! Cloud Files "in sync" state in step with [`SyncEngine::path_state`].

use std::collections::BTreeSet;
use std::path::Path;

use bsync_cloudfiles::SyncRoot;
use bsync_engine::{PathState, SyncEngine};

/// Paths updated per engine-loop turn, so a first pass over a big folder
/// does not hold up syncing.
pub const BATCH: usize = 500;

#[derive(Debug)]
pub struct Badges {
    root: SyncRoot,
    /// Record paths whose badge may be out of date.
    queue: BTreeSet<String>,
}

impl Badges {
    pub fn start(path: &Path, folder_id: &str) -> Option<Self> {
        match SyncRoot::register(path, folder_id) {
            Ok(root) => Some(Self {
                root,
                queue: BTreeSet::new(),
            }),
            Err(e) => {
                tracing::warn!(folder = %folder_id, "Explorer badges unavailable: {e}");
                None
            }
        }
    }

    /// Forget the registration of a folder that is no longer synced.
    pub fn remove(path: &Path) {
        if let Err(e) = SyncRoot::unregister(path) {
            tracing::warn!("{e}");
        }
    }

    /// Bring up to [`BATCH`] badges up to date; true if more are waiting.
    pub fn update(&mut self, engine: &mut SyncEngine) -> bool {
        match engine.take_touched() {
            Ok(touched) => {
                for p in touched {
                    // A change inside a directory can change its badge too.
                    let mut cur = p.as_str();
                    while let Some(parent) = bsync_core::parent_of(cur) {
                        self.queue.insert(parent.to_owned());
                        cur = parent;
                    }
                    self.queue.insert(p);
                }
                self.queue.insert(String::new());
            }
            Err(e) => tracing::warn!("badge update: {e}"),
        }
        for _ in 0..BATCH {
            let Some(rel) = self.queue.pop_first() else {
                break;
            };
            let in_sync = match engine.path_state(&rel) {
                Ok(PathState::Synced) => true,
                Ok(PathState::Ignored | PathState::Unknown) => continue,
                Ok(_) => false,
                Err(e) => {
                    tracing::debug!(%rel, "badge state: {e}");
                    continue;
                }
            };
            // Files open elsewhere or vanished meanwhile: the next change
            // brings them back here.
            if let Err(e) = self.root.set_in_sync(&engine.folder().abs(&rel), in_sync) {
                tracing::debug!(%rel, "badge: {e}");
            }
        }
        !self.queue.is_empty()
    }
}
