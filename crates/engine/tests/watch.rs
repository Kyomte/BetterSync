//! M2: changes in a real directory reach the index within debounce + 500 ms.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bsync_core::{ChunkParams, DeviceId, Hash};
use bsync_engine::scan::{ScanContext, scan};
use bsync_fs::{FolderWatcher, Metadata, StdFs, WatchConfig, WatchEvent};
use bsync_index::Index;

const DEBOUNCE: Duration = Duration::from_millis(500);

fn no_skip(_: &str, _: &Metadata) -> bool {
    false
}

struct Harness {
    root: tempfile::TempDir,
    index: Index,
    events: mpsc::Receiver<WatchEvent>,
    _watcher: FolderWatcher,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut index = Index::open_in_memory().unwrap();
        scan(&ctx(root.path()), &mut index, "").unwrap();
        let (tx, events) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let watcher = FolderWatcher::new(
            root.path(),
            WatchConfig {
                debounce: DEBOUNCE,
                full_rescan: Duration::from_secs(3600),
            },
            move |e| {
                let _ = tx.lock().unwrap().send(e);
            },
        )
        .unwrap();
        // Give the OS watcher a moment to arm (FSEvents in particular).
        std::thread::sleep(Duration::from_millis(200));
        Self {
            root,
            index,
            events,
            _watcher: watcher,
        }
    }

    /// Feed watcher events into scans until `done` holds, failing if that
    /// takes longer than debounce + 500 ms from `start`.
    fn wait_until(&mut self, start: Instant, what: &str, done: impl Fn(&Index) -> bool) {
        let deadline = start + DEBOUNCE + Duration::from_millis(500);
        while !done(&self.index) {
            let now = Instant::now();
            assert!(now < deadline, "index not updated in time: {what}");
            let Ok(event) = self.events.recv_timeout(deadline - now) else {
                continue;
            };
            let root = self.root.path().to_path_buf();
            match event {
                WatchEvent::Paths(paths) => {
                    for p in paths {
                        scan(&ctx(&root), &mut self.index, &p).unwrap();
                    }
                }
                WatchEvent::FullRescan => {
                    scan(&ctx(&root), &mut self.index, "").unwrap();
                }
            }
        }
    }
}

fn ctx(root: &Path) -> ScanContext<'_> {
    ScanContext {
        fs: &StdFs,
        root,
        device: DeviceId::from_bytes([1; 32]),
        params: ChunkParams::DEFAULT,
        skip: &no_skip,
    }
}

fn live_hash(index: &Index, path: &str) -> Option<Hash> {
    index
        .get(path)
        .unwrap()
        .filter(|e| !e.record.deleted)
        .map(|e| e.record.hash)
}

#[test]
fn create_modify_rename_delete_reach_the_index() {
    let mut h = Harness::new();
    let root = h.root.path().to_path_buf();

    let t = Instant::now();
    std::fs::create_dir(root.join("dir")).unwrap();
    std::fs::write(root.join("dir/a.txt"), b"first").unwrap();
    h.wait_until(t, "create", |i| {
        live_hash(i, "dir/a.txt") == Some(Hash::of(b"first"))
    });
    assert!(h.index.get("dir").unwrap().unwrap().record.is_live_dir());

    let t = Instant::now();
    std::fs::write(root.join("dir/a.txt"), b"second version").unwrap();
    h.wait_until(t, "modify", |i| {
        live_hash(i, "dir/a.txt") == Some(Hash::of(b"second version"))
    });

    let t = Instant::now();
    std::fs::rename(root.join("dir/a.txt"), root.join("b.txt")).unwrap();
    h.wait_until(t, "rename", |i| {
        live_hash(i, "dir/a.txt").is_none()
            && live_hash(i, "b.txt") == Some(Hash::of(b"second version"))
    });

    let t = Instant::now();
    std::fs::remove_file(root.join("b.txt")).unwrap();
    std::fs::remove_dir(root.join("dir")).unwrap();
    h.wait_until(t, "delete", |i| {
        live_hash(i, "b.txt").is_none() && i.get("dir").unwrap().unwrap().record.deleted
    });
}

#[test]
fn burst_of_writes_is_debounced_into_final_content() {
    let mut h = Harness::new();
    let root = h.root.path().to_path_buf();
    let t0 = Instant::now();
    for i in 0..20 {
        std::fs::write(root.join("burst"), format!("write {i}")).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    let t = Instant::now();
    h.wait_until(t, "burst", |i| {
        live_hash(i, "burst") == Some(Hash::of(b"write 19"))
    });
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn editor_safe_saves_end_as_a_plain_edit() {
    let mut h = Harness::new();
    let root = h.root.path().to_path_buf();
    std::fs::write(root.join("doc.txt"), b"v1").unwrap();
    let t = Instant::now();
    h.wait_until(t, "create", |i| {
        live_hash(i, "doc.txt") == Some(Hash::of(b"v1"))
    });
    let v1 = h.index.get("doc.txt").unwrap().unwrap().record.version;

    // VS Code / vim style: write a temp file, rename it over the original.
    let t = Instant::now();
    std::fs::write(root.join("doc.txt.swp"), b"v2").unwrap();
    std::fs::rename(root.join("doc.txt.swp"), root.join("doc.txt")).unwrap();
    h.wait_until(t, "rename-over save", |i| {
        live_hash(i, "doc.txt") == Some(Hash::of(b"v2")) && live_hash(i, "doc.txt.swp").is_none()
    });

    // Word style: delete, then recreate.
    let t = Instant::now();
    std::fs::remove_file(root.join("doc.txt")).unwrap();
    std::fs::write(root.join("doc.txt"), b"v3").unwrap();
    h.wait_until(t, "delete+recreate save", |i| {
        live_hash(i, "doc.txt") == Some(Hash::of(b"v3"))
    });
    let v3 = h.index.get("doc.txt").unwrap().unwrap().record.version;
    assert!(v3.dominates(&v1), "history continues across the save");
}
