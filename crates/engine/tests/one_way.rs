//! M3: two temp dirs, A → B, reach byte-identical state across a scripted
//! set of creates, edits, deletes and renames; the versions folder holds
//! replaced content.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bsync_core::{ChunkParams, DeviceId, FileRecord, Hash};
use bsync_engine::{ChunkSource, Folder, SyncStats, SystemClock};
use bsync_fs::StdFs;
use bsync_index::Index;

const A: DeviceId = DeviceId::from_bytes([0xA; 32]);
const B: DeviceId = DeviceId::from_bytes([0xB; 32]);

fn folder(root: &Path, device: DeviceId) -> Folder {
    Folder::new(
        Arc::new(StdFs),
        Arc::new(SystemClock),
        root.to_path_buf(),
        device,
        // Small chunks so modest test files have many of them.
        ChunkParams {
            min: 1024,
            avg: 4096,
            max: 16384,
        },
        Index::open_in_memory().unwrap(),
    )
}

/// Serves chunks out of another folder's files.
struct FolderSource<'a>(&'a Folder);

impl ChunkSource for FolderSource<'_> {
    fn has(&self, hash: &Hash) -> bool {
        !self.0.index().find_chunk(hash).unwrap().is_empty()
    }
    fn get(&mut self, hash: &Hash) -> Option<Vec<u8>> {
        self.0.local_chunk(hash).unwrap()
    }
}

/// Relative path → content (None for directories), excluding `.bsync`.
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with(".bsync") {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                stack.push(e.path());
            } else {
                out.insert(rel, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

fn versions(root: &Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut stack = vec![root.join(".bsync/versions")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(std::fs::read(e.path()).unwrap());
            }
        }
    }
    out.sort();
    out
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

struct Pair {
    _ta: tempfile::TempDir,
    _tb: tempfile::TempDir,
    a: Folder,
    b: Folder,
}

impl Pair {
    fn new() -> Self {
        let (ta, tb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (folder(ta.path(), A), folder(tb.path(), B));
        Self {
            _ta: ta,
            _tb: tb,
            a,
            b,
        }
    }

    fn ra(&self) -> &Path {
        self.a.root()
    }

    /// Scan A, sync everything to B, and check the trees match.
    fn sync(&mut self) -> SyncStats {
        self.a.scan("").unwrap();
        self.b.scan("").unwrap();
        let records: Vec<FileRecord> = self
            .a
            .index()
            .all()
            .unwrap()
            .into_iter()
            .map(|e| e.record)
            .collect();
        let stats = self
            .b
            .sync_from(A, &records, &mut FolderSource(&self.a))
            .unwrap();
        assert!(stats.blocked.is_empty(), "blocked: {:?}", stats.blocked);
        assert_eq!(tree(self.a.root()), tree(self.b.root()));
        // B is fully caught up: nothing left to do.
        assert!(self.b.plan_all().unwrap().is_empty());
        stats
    }
}

#[test]
fn scripted_one_way_sync() {
    let mut p = Pair::new();
    let ra = p.ra().to_path_buf();
    let w = |rel: &str, content: &[u8]| std::fs::write(ra.join(rel), content).unwrap();

    // Creates, including nested and empty directories and an empty file.
    std::fs::create_dir_all(ra.join("docs/deep/er")).unwrap();
    std::fs::create_dir_all(ra.join("empty")).unwrap();
    w("docs/a.txt", b"alpha");
    w("docs/deep/er/b.bin", &data(100_000, 1));
    w("zero", b"");
    let s = p.sync();
    assert_eq!(s.chunks_reused, 0);
    assert!(s.chunks_fetched > 10);

    // Edit the middle of the big file: only a few chunks are fetched.
    let mut big = data(100_000, 1);
    big[50_000..50_100].fill(7);
    w("docs/deep/er/b.bin", &big);
    w("docs/a.txt", b"alpha, edited");
    let s = p.sync();
    assert!(s.chunks_fetched <= 4, "fetched {}", s.chunks_fetched);
    assert!(s.chunks_reused > 10);
    let rb = p.b.root().to_path_buf();
    let old = versions(&rb);
    assert!(
        old.contains(&b"alpha".to_vec()),
        "replaced content is in versions/"
    );
    assert!(old.contains(&data(100_000, 1)));

    // Rename: the content is rebuilt from B's own copy, nothing fetched.
    std::fs::rename(ra.join("docs/deep/er/b.bin"), ra.join("moved.bin")).unwrap();
    let s = p.sync();
    assert_eq!(s.chunks_fetched, 0);

    // Deletes, including a whole directory tree.
    std::fs::remove_dir_all(ra.join("docs")).unwrap();
    std::fs::remove_file(ra.join("zero")).unwrap();
    p.sync();
    assert!(versions(&rb).contains(&b"alpha, edited".to_vec()));

    // File replaced by a directory of the same name, and back.
    std::fs::remove_file(ra.join("moved.bin")).unwrap();
    std::fs::create_dir(ra.join("moved.bin")).unwrap();
    w("moved.bin/inner", b"inside");
    p.sync();
    std::fs::remove_dir_all(ra.join("moved.bin")).unwrap();
    w("moved.bin", b"a file again");
    p.sync();

    // Re-create a previously deleted path.
    std::fs::create_dir(ra.join("docs")).unwrap();
    w("docs/a.txt", b"back");
    p.sync();
}

#[test]
fn local_change_on_target_is_not_overwritten() {
    let mut p = Pair::new();
    let ra = p.ra().to_path_buf();
    std::fs::write(ra.join("f"), b"v1").unwrap();
    p.sync();

    // A edits f; B also edits f but has not scanned yet. Applying A's
    // version must notice B's unscanned edit (invariant 3) and keep it.
    std::fs::write(ra.join("f"), b"from A").unwrap();
    p.a.scan("").unwrap();
    let rb = p.b.root().to_path_buf();
    std::fs::write(rb.join("f"), b"from B, unscanned").unwrap();
    let records: Vec<FileRecord> =
        p.a.index()
            .all()
            .unwrap()
            .into_iter()
            .map(|e| e.record)
            .collect();
    p.b.sync_from(A, &records, &mut FolderSource(&p.a)).unwrap();

    let t = tree(&rb);
    let contents: Vec<_> = t.values().flatten().cloned().collect();
    assert!(
        contents.contains(&b"from A".to_vec()) || contents.contains(&b"from B, unscanned".to_vec())
    );
    assert!(
        contents.contains(&b"from B, unscanned".to_vec()),
        "B's local edit survives as a file: {t:?}"
    );
}
