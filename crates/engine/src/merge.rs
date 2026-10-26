//! Merging concurrent edits of text files (stretch goal S2).
//!
//! Every version carries the hashes of the versions it descends from
//! (`ancestors`). Each device keeps the content of recent small text
//! versions in `.bsync/bases/<hash>`, so when two edits conflict, the device
//! holding the losing side can find their closest common ancestor and do a
//! three-way, line-based merge. A
//! clean merge replaces the conflict copy; overlapping edits still produce
//! one.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use bsync_core::{FileRecord, Hash};
use bsync_fs::{BSYNC_DIR, FileSystem, rel_to_path};

/// Only files up to this size are merged (and kept as bases).
pub const MERGE_MAX: u64 = 1024 * 1024;

/// Text worth merging: small, UTF-8, no NUL bytes.
pub fn is_text(data: &[u8]) -> bool {
    data.len() as u64 <= MERGE_MAX && !data.contains(&0) && std::str::from_utf8(data).is_ok()
}

/// Three-way merge. `None` if the two sides changed the same lines.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let (Ok(base), Ok(ours), Ok(theirs)) = (
        std::str::from_utf8(base),
        std::str::from_utf8(ours),
        std::str::from_utf8(theirs),
    ) else {
        return None;
    };
    diffy::merge(base, ours, theirs)
        .ok()
        .map(String::into_bytes)
}

/// The store of ancestor versions in `.bsync/bases/`.
pub struct Bases<'a> {
    fs: &'a dyn FileSystem,
    root: &'a Path,
}

impl std::fmt::Debug for Bases<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bases").field("root", &self.root).finish()
    }
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

impl<'a> Bases<'a> {
    pub fn new(fs: &'a dyn FileSystem, root: &'a Path) -> Self {
        Self { fs, root }
    }

    fn rel(hash: &Hash) -> String {
        format!("{BSYNC_DIR}/bases/{}", hash.to_hex())
    }

    fn path(&self, rel: &str) -> PathBuf {
        rel_to_path(self.root, rel)
    }

    fn ensure_dir(&self) -> io::Result<()> {
        for d in [BSYNC_DIR.to_owned(), format!("{BSYNC_DIR}/bases")] {
            match self.fs.create_dir(&self.path(&d)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Write `name` atomically if it does not exist yet.
    fn put(&self, rel: &str, data: &[u8]) -> io::Result<()> {
        if self.fs.metadata(&self.path(rel)).is_ok() {
            return Ok(());
        }
        let tmp = format!("{rel}.tmp");
        let _ = self.fs.remove_file(&self.path(&tmp));
        self.fs.create_new(&self.path(&tmp), data)?;
        self.fs.rename(&self.path(&tmp), &self.path(rel))
    }

    /// Keep `content` (if it is mergeable text) as a possible future base.
    pub fn store(&self, hash: &Hash, content: &[u8]) -> io::Result<()> {
        if !is_text(content) || *hash == Hash::default() {
            return Ok(());
        }
        self.ensure_dir()?;
        self.put(&Self::rel(hash), content)
    }

    pub fn load(&self, hash: &Hash) -> Option<Vec<u8>> {
        let data = self.fs.read(&self.path(&Self::rel(hash))).ok()?;
        (Hash::of(&data) == *hash).then_some(data)
    }

    /// The closest common ancestor of two concurrent versions whose content
    /// is stored here.
    pub fn common_ancestor(&self, a: &FileRecord, b: &FileRecord) -> Option<Vec<u8>> {
        let theirs: BTreeSet<Hash> = b.ancestors.iter().copied().collect();
        a.ancestors
            .iter()
            .filter(|h| theirs.contains(h))
            .find_map(|h| self.load(h))
    }

    /// Delete stored bases no current version can lead back to.
    pub fn prune<'r>(
        &self,
        records: impl IntoIterator<Item = &'r FileRecord>,
    ) -> io::Result<usize> {
        let dir = self.path(&format!("{BSYNC_DIR}/bases"));
        let listing = match self.fs.read_dir(&dir) {
            Ok(l) => l,
            Err(e) if not_found(&e) => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut keep = BTreeSet::new();
        for r in records {
            keep.insert(r.hash);
            keep.extend(r.ancestors.iter().copied());
        }
        let keep: BTreeSet<String> = keep.iter().map(Hash::to_hex).collect();
        let mut removed = 0;
        for e in listing {
            let name = e.name.to_string_lossy();
            let stem = name.trim_end_matches(".tmp");
            if !keep.contains(stem) {
                let _ = self.fs.remove_file(&dir.join(&e.name));
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_disjoint_edits_and_refuses_overlaps() {
        let base = b"one\ntwo\nthree\nfour\n";
        let ours = b"ONE\ntwo\nthree\nfour\n";
        let theirs = b"one\ntwo\nthree\nFOUR\n";
        assert_eq!(
            merge_text(base, ours, theirs).unwrap(),
            b"ONE\ntwo\nthree\nFOUR\n"
        );
        let clash = b"one\nTWO?\nthree\nfour\n";
        let clash2 = b"one\nTWO!\nthree\nfour\n";
        assert!(merge_text(base, clash, clash2).is_none());
        assert!(merge_text(base, &[0xff], theirs).is_none());
    }

    #[test]
    fn text_detection() {
        assert!(is_text(b"plain\n"));
        assert!(is_text("unicode ✓\n".as_bytes()));
        assert!(!is_text(b"nul\0byte"));
        assert!(!is_text(&[0xff, 0xfe]));
        assert!(!is_text(&vec![b'a'; MERGE_MAX as usize + 1]));
    }
}
