//! OS file-change notifications, debounced, turned into "rescan these
//! relative paths" events. A slow periodic full rescan covers anything the
//! OS fails to report.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, NoCache, new_debouncer_opt};

use crate::is_internal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// How long a path must be quiet before it is reported. Editors that save
    /// by delete + recreate or write in bursts settle within this.
    pub debounce: Duration,
    /// Full rescan interval, in case notifications were missed.
    pub full_rescan: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(1),
            full_rescan: Duration::from_secs(60 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// These relative paths (and anything below them) may have changed.
    /// Ancestors are listed before descendants and descendants of a listed
    /// path are dropped.
    Paths(Vec<String>),
    /// Rescan the whole folder (periodic, or notifications were lost).
    FullRescan,
}

/// Watches a folder until dropped.
pub struct FolderWatcher {
    _debouncer: Box<dyn std::any::Any + Send>,
    stop_timer: mpsc::Sender<()>,
}

impl std::fmt::Debug for FolderWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderWatcher").finish_non_exhaustive()
    }
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        let _ = self.stop_timer.send(());
    }
}

/// Reduce absolute event paths to sorted relative paths under `roots`,
/// dropping internal paths and paths covered by an ancestor in the set.
pub fn relative_paths(roots: &[PathBuf], paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    let mut set = BTreeSet::new();
    for p in paths {
        let Some(rel) = roots.iter().find_map(|r| p.strip_prefix(r).ok()) else {
            continue;
        };
        let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
        let Some(parts) = parts else { continue };
        let rel = parts.join("/");
        if is_internal(&rel) {
            continue;
        }
        set.insert(rel);
    }
    let mut out: Vec<String> = Vec::new();
    for p in set {
        let covered = out
            .iter()
            .any(|a| a.is_empty() || p == *a || p.starts_with(&format!("{a}/")));
        if !covered {
            out.push(p);
        }
    }
    out
}

impl FolderWatcher {
    /// Start watching `root`. `on_event` runs on a background thread.
    pub fn new(
        root: &Path,
        config: WatchConfig,
        on_event: impl Fn(WatchEvent) + Send + Sync + 'static,
    ) -> notify::Result<Self> {
        // Events may name either the path as given or its canonical form
        // (e.g. /var → /private/var on macOS).
        let mut roots = vec![root.to_path_buf()];
        if let Ok(c) = root.canonicalize()
            && c != root
        {
            roots.push(c);
        }
        let on_event = std::sync::Arc::new(on_event);
        let handler = {
            let on_event = on_event.clone();
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for e in &events {
                        tracing::debug!(kind = ?e.kind, paths = ?e.paths, "fs event");
                    }
                    // A rename reported with only one of its two paths
                    // (FSEvents does this) hides where the file went or came
                    // from; only a full rescan finds the other side. Rescans
                    // only re-hash changed files, so this is cheap.
                    let half_rename = |e: &notify_debouncer_full::DebouncedEvent| {
                        matches!(e.kind, EventKind::Modify(ModifyKind::Name(_)))
                            && e.paths.len() < 2
                    };
                    if events.iter().any(|e| e.need_rescan() || half_rename(e)) {
                        on_event(WatchEvent::FullRescan);
                        return;
                    }
                    let paths =
                        relative_paths(&roots, events.into_iter().flat_map(|e| e.event.paths));
                    if !paths.is_empty() {
                        on_event(WatchEvent::Paths(paths));
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!("file watcher error: {e}");
                    }
                    on_event(WatchEvent::FullRescan);
                }
            }
        };
        // No file-ID cache: with it, the debouncer "stitches" FSEvents
        // renames into a bare Create of the new name and drops the old one
        // (seen on macOS CI). Without it renames arrive as Name events,
        // handled above. inotify and Windows report both names anyway.
        let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
            config.debounce,
            None,
            handler,
            NoCache,
            notify::Config::default(),
        )?;
        debouncer.watch(root, RecursiveMode::Recursive)?;

        let (stop_timer, stopped) = mpsc::channel::<()>();
        let interval = config.full_rescan;
        thread::Builder::new()
            .name("bsync-rescan-timer".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    on_event(WatchEvent::FullRescan);
                }
            })
            .map_err(|e| notify::Error::generic(&e.to_string()))?;

        Ok(Self {
            _debouncer: Box::new(debouncer),
            stop_timer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_dedupes_and_filters() {
        let root = PathBuf::from("/r");
        let got = relative_paths(
            std::slice::from_ref(&root),
            [
                root.join("a/b"),
                root.join("a"),
                root.join("a/c"),
                root.join("ab"),
                root.join(".bsync/index"),
                root.join("x/.bsync-tmp-1"),
                PathBuf::from("/elsewhere/z"),
            ],
        );
        assert_eq!(got, vec!["a", "ab"]);
        assert_eq!(
            relative_paths(std::slice::from_ref(&root), [root.clone(), root.join("q")]),
            vec![""]
        );
    }
}
