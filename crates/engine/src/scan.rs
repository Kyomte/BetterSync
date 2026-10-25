//! Local scanning: compare the folder on disk with the index and record
//! local changes as new versions.
//!
//! Only files whose size or mtime differ from what the index remembers are
//! re-hashed.

use std::collections::BTreeSet;
use std::path::Path;

use bsync_core::{ChunkParams, DeviceId, EntryKind, FileRecord, Hash, chunk_reader};
use bsync_fs::{FileKind, FileSystem, Metadata, WalkEntry, mtime_ns, rel_to_path, walk};
use bsync_index::{Index, LocalEntry, LocalStat};

use crate::Result;

/// What a scan needs to know about the folder and device.
#[derive(Clone, Copy)]
pub struct ScanContext<'a> {
    pub fs: &'a dyn FileSystem,
    pub root: &'a Path,
    pub device: DeviceId,
    /// Lowest counter value a new local version may get (current Unix time
    /// in seconds); see [`Index::next_own_counter`].
    pub counter_floor: u64,
    pub params: ChunkParams,
    /// Extra paths to leave out (ignore rules).
    pub skip: &'a dyn Fn(&str, &Metadata) -> bool,
}

impl std::fmt::Debug for ScanContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanContext")
            .field("root", &self.root)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub hashed_files: u64,
    pub hashed_bytes: u64,
    /// Paths whose local record got a new version.
    pub changed: Vec<String>,
}

pub fn stat_of(meta: &Metadata) -> LocalStat {
    LocalStat {
        size: meta.len,
        mtime_ns: mtime_ns(meta.modified),
    }
}

/// A new local version of `path` replacing `previous` (if any).
pub fn new_version(
    previous: Option<&FileRecord>,
    path: &str,
    device: DeviceId,
    counter_floor: u64,
) -> FileRecord {
    FileRecord {
        path: path.to_owned(),
        kind: previous.map_or(EntryKind::File, |p| p.kind),
        deleted: false,
        size: 0,
        mtime_ns: 0,
        version: previous
            .map(|p| p.version.clone())
            .unwrap_or_default()
            .incremented_at_least(device, counter_floor),
        modified_by: device,
        hash: Hash::default(),
        chunks: Vec::new(),
    }
}

/// A tombstone recording that `previous` was deleted on `device`.
pub fn tombstone(previous: &FileRecord, device: DeviceId, counter_floor: u64) -> FileRecord {
    let mut r = new_version(Some(previous), &previous.path, device, counter_floor);
    r.deleted = true;
    r
}

/// Hash a file, retrying if it changes while being read. Returns `None` if
/// it keeps changing or vanished (a later scan picks it up).
pub fn hash_file(
    ctx: &ScanContext<'_>,
    path: &str,
    meta: &Metadata,
    stats: &mut ScanStats,
) -> Result<Option<(bsync_core::ChunkedFile, Metadata)>> {
    let abs = rel_to_path(ctx.root, path);
    let mut before = meta.clone();
    for _ in 0..3 {
        let reader = match ctx.fs.open_read(&abs) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let chunked = chunk_reader(reader, ctx.params)?;
        stats.hashed_files += 1;
        stats.hashed_bytes += chunked.size;
        let after = match ctx.fs.metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if after == before && chunked.size == after.len {
            return Ok(Some((chunked, after)));
        }
        before = after;
    }
    Ok(None)
}

/// Bring the index up to date with one walked entry. Returns whether the
/// local record got a new version.
fn update_entry(
    ctx: &ScanContext<'_>,
    index: &Index,
    entry: &WalkEntry,
    existing: Option<&LocalEntry>,
    stats: &mut ScanStats,
) -> Result<bool> {
    let prev = existing.map(|e| &e.record);
    match entry.metadata.kind {
        FileKind::Dir => {
            if prev.is_some_and(FileRecord::is_live_dir) {
                return Ok(false);
            }
            let mut r = new_version(
                prev,
                &entry.path,
                ctx.device,
                index.next_own_counter(ctx.counter_floor)?,
            );
            r.kind = EntryKind::Dir;
            r.mtime_ns = mtime_ns(entry.metadata.modified);
            index.put(&r, None)?;
            Ok(true)
        }
        FileKind::File => {
            let stat = stat_of(&entry.metadata);
            if let Some(e) = existing {
                if e.record.is_live_file() && e.stat == Some(stat) {
                    return Ok(false);
                }
            }
            let Some((chunked, meta)) = hash_file(ctx, &entry.path, &entry.metadata, stats)? else {
                return Ok(false);
            };
            if let Some(e) = existing {
                if e.record.is_live_file() && e.record.hash == chunked.hash {
                    // Touched but not changed.
                    index.set_stat(&entry.path, Some(stat_of(&meta)))?;
                    return Ok(false);
                }
            }
            let mut r = new_version(
                prev,
                &entry.path,
                ctx.device,
                index.next_own_counter(ctx.counter_floor)?,
            );
            r.kind = EntryKind::File;
            r.size = chunked.size;
            r.mtime_ns = mtime_ns(meta.modified);
            r.hash = chunked.hash;
            r.chunks = chunked.chunks;
            index.put(&r, Some(stat_of(&meta)))?;
            Ok(true)
        }
        FileKind::Symlink => Ok(false),
    }
}

/// Scan `subtree` (relative path, `""` for the whole folder) and record
/// local changes in `index`.
pub fn scan(ctx: &ScanContext<'_>, index: &mut Index, subtree: &str) -> Result<ScanStats> {
    let walked = match walk(ctx.fs, ctx.root, subtree, ctx.skip) {
        Ok(w) => w,
        // The subtree itself is gone: everything under it was deleted.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !subtree.is_empty() => {
            Default::default()
        }
        Err(e) => return Err(e.into()),
    };
    index.batch(|index| {
        let mut stats = ScanStats::default();
        // A subtree scan may be the first sign of new parent directories.
        let mut ancestors = Vec::new();
        let mut cur = subtree;
        while let Some(parent) = bsync_core::parent_of(cur) {
            ancestors.push(parent);
            cur = parent;
        }
        for dir in ancestors.into_iter().rev() {
            match ctx.fs.metadata(&rel_to_path(ctx.root, dir)) {
                Ok(metadata) if metadata.kind == FileKind::Dir => {
                    let entry = WalkEntry {
                        path: dir.to_owned(),
                        metadata,
                    };
                    let existing = index.get(dir)?;
                    if update_entry(ctx, index, &entry, existing.as_ref(), &mut stats)? {
                        stats.changed.push(dir.to_owned());
                    }
                }
                _ => break,
            }
        }
        let mut seen = BTreeSet::new();
        for entry in &walked.entries {
            seen.insert(entry.path.as_str());
            let existing = index.get(&entry.path)?;
            if update_entry(ctx, index, entry, existing.as_ref(), &mut stats)? {
                tracing::debug!(device = ?ctx.device, path = %entry.path, "scan: changed");
                stats.changed.push(entry.path.clone());
            }
        }
        let unreadable = |path: &str| {
            walked.errors.iter().any(|(dir, _)| {
                dir.is_empty() || path == dir || path.starts_with(&format!("{dir}/"))
            })
        };
        for e in index.under(subtree)? {
            let path = e.record.path.as_str();
            if e.record.deleted || seen.contains(path) || unreadable(path) {
                continue;
            }
            // Ignored paths are not ours to delete; they simply stop syncing.
            if (ctx.skip)(path, &placeholder_meta(&e.record)) {
                continue;
            }
            tracing::debug!(device = ?ctx.device, path = %e.record.path, "scan: deleted");
            let counter = index.next_own_counter(ctx.counter_floor)?;
            index.put(&tombstone(&e.record, ctx.device, counter), None)?;
            stats.changed.push(e.record.path.clone());
        }
        Ok(stats)
    })
}

/// Metadata to ask ignore rules about a path that no longer exists.
fn placeholder_meta(record: &FileRecord) -> Metadata {
    Metadata {
        kind: match record.kind {
            EntryKind::Dir => FileKind::Dir,
            EntryKind::File => FileKind::File,
        },
        len: record.size,
        modified: std::time::SystemTime::UNIX_EPOCH,
    }
}

#[cfg(test)]
mod tests {
    use bsync_fs::StdFs;

    use super::*;

    const DEV: DeviceId = DeviceId::from_bytes([7; 32]);

    fn ctx<'a>(root: &'a Path, skip: &'a dyn Fn(&str, &Metadata) -> bool) -> ScanContext<'a> {
        ScanContext {
            fs: &StdFs,
            root,
            device: DEV,
            counter_floor: 0,
            params: ChunkParams::DEFAULT,
            skip,
        }
    }

    fn no_skip(_: &str, _: &Metadata) -> bool {
        false
    }

    fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    fn write(root: &Path, rel: &str, data: &[u8]) {
        std::fs::write(rel_to_path(root, rel), data).unwrap();
    }

    #[test]
    fn scan_fixture_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::create_dir_all(root.join(".bsync/versions")).unwrap();
        write(root, "docs/a.txt", b"alpha");
        write(root, "docs/sub/b.txt", b"beta");
        write(root, "top.bin", &[]);
        write(root, ".bsync/versions/old", b"x");

        let mut index = Index::open_in_memory().unwrap();
        let stats = scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        assert_eq!(stats.hashed_files, 3);
        let all = index.all().unwrap();
        let paths: Vec<_> = all.iter().map(|e| e.record.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "docs",
                "docs/a.txt",
                "docs/sub",
                "docs/sub/b.txt",
                "top.bin"
            ]
        );

        let a = index.get("docs/a.txt").unwrap().unwrap().record;
        assert_eq!(a.hash, Hash::of(b"alpha"));
        assert_eq!(a.size, 5);
        assert!(a.version.get(&DEV) > 0);
        assert_eq!(a.modified_by, DEV);
        assert!(index.get("docs").unwrap().unwrap().record.is_live_dir());
        assert_eq!(index.max_seq().unwrap(), 5);
    }

    #[test]
    fn rescan_without_changes_does_no_hashing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        write(root, "d/x", &pseudo_random(200_000, 1));
        write(root, "y", b"why");
        let mut index = Index::open_in_memory().unwrap();
        scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        let seq = index.max_seq().unwrap();

        let stats = scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        assert_eq!(stats, ScanStats::default());
        assert_eq!(index.max_seq().unwrap(), seq);
    }

    #[test]
    fn edits_deletes_and_recreates_bump_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        write(root, "d/x", b"one");
        write(root, "y", b"why");
        let mut index = Index::open_in_memory().unwrap();
        scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        let y1 = index.get("y").unwrap().unwrap().record.version;
        let x1 = index.get("d/x").unwrap().unwrap().record.version;

        write(root, "y", b"why not");
        std::fs::remove_dir_all(root.join("d")).unwrap();
        let stats = scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        assert_eq!(stats.changed, vec!["y", "d", "d/x"]);
        let y = index.get("y").unwrap().unwrap().record;
        assert_eq!(y.version.compare(&y1), bsync_core::Causality::Newer);
        let x = index.get("d/x").unwrap().unwrap().record;
        assert!(x.deleted);
        assert_eq!(x.version.compare(&x1), bsync_core::Causality::Newer);
        let x2 = x.version;
        assert!(index.find_chunk(&Hash::of(b"one")).unwrap().is_empty());

        // Recreating continues the same version history.
        std::fs::create_dir(root.join("d")).unwrap();
        write(root, "d/x", b"two");
        scan(&ctx(root, &no_skip), &mut index, "d").unwrap();
        let x = index.get("d/x").unwrap().unwrap().record;
        assert!(!x.deleted);
        assert_eq!(x.version.compare(&x2), bsync_core::Causality::Newer);
    }

    #[test]
    fn subtree_scan_of_vanished_dir_tombstones_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        write(root, "d/x", b"one");
        write(root, "keep", b"k");
        let mut index = Index::open_in_memory().unwrap();
        scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        std::fs::remove_dir_all(root.join("d")).unwrap();
        let stats = scan(&ctx(root, &no_skip), &mut index, "d").unwrap();
        assert_eq!(stats.changed, vec!["d", "d/x"]);
        assert!(!index.get("keep").unwrap().unwrap().record.deleted);
    }

    #[test]
    fn subtree_scan_records_new_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut index = Index::open_in_memory().unwrap();
        std::fs::create_dir_all(root.join("p/q")).unwrap();
        write(root, "p/q/f", b"f");
        let stats = scan(&ctx(root, &no_skip), &mut index, "p/q/f").unwrap();
        assert_eq!(stats.changed, vec!["p", "p/q", "p/q/f"]);
    }

    #[test]
    fn middle_edit_of_10mb_file_changes_only_a_few_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut data = pseudo_random(10 * 1024 * 1024, 3);
        write(root, "big", &data);
        let mut index = Index::open_in_memory().unwrap();
        scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        let before = index.get("big").unwrap().unwrap().record;

        let mid = data.len() / 2;
        data[mid..mid + 4096].fill(0);
        write(root, "big", &data);
        let stats = scan(&ctx(root, &no_skip), &mut index, "").unwrap();
        assert_eq!(stats.changed, vec!["big"]);
        let after = index.get("big").unwrap().unwrap().record;
        let old: BTreeSet<_> = before.chunks.iter().map(|c| c.hash).collect();
        let new_chunks = after
            .chunks
            .iter()
            .filter(|c| !old.contains(&c.hash))
            .count();
        assert!(
            (1..=3).contains(&new_chunks),
            "{new_chunks} of {} chunks changed",
            after.chunks.len()
        );
    }
}
