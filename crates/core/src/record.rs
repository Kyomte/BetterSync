use serde::{Deserialize, Serialize};

use crate::{DeviceId, Hash, VersionVector};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
}

/// One content-defined chunk of a file. Offsets follow from the order and
/// lengths of the chunks in a [`FileRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u32,
}

/// What every device knows about one path in a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Relative path, `/`-separated, Unicode NFC.
    pub path: String,
    pub kind: EntryKind,
    /// A tombstone: the entry was deleted. Kept so the deletion syncs.
    pub deleted: bool,
    pub size: u64,
    /// Modification time (ns since the Unix epoch) on the device that made
    /// this version. Informational only — never used for ordering.
    pub mtime_ns: i64,
    pub version: VersionVector,
    /// The device whose change produced this version.
    pub modified_by: DeviceId,
    /// BLAKE3 of the whole content; default for directories and tombstones.
    pub hash: Hash,
    pub chunks: Vec<ChunkRef>,
    /// Content hashes of the versions this one descends from, nearest
    /// first (at most [`MAX_ANCESTORS`]): lets devices find a common
    /// ancestor for merging concurrent edits of text files.
    pub ancestors: Vec<Hash>,
}

impl FileRecord {
    /// Whether two records describe the same thing on disk, ignoring their
    /// version history.
    pub fn same_content(&self, other: &Self) -> bool {
        if self.deleted || other.deleted {
            return self.deleted == other.deleted;
        }
        self.kind == other.kind && (self.kind == EntryKind::Dir || self.hash == other.hash)
    }

    pub fn is_live_file(&self) -> bool {
        !self.deleted && self.kind == EntryKind::File
    }

    pub fn is_live_dir(&self) -> bool {
        !self.deleted && self.kind == EntryKind::Dir
    }

    /// `(offset, chunk)` for every chunk.
    pub fn chunk_offsets(&self) -> impl Iterator<Item = (u64, &ChunkRef)> {
        self.chunks.iter().scan(0u64, |offset, c| {
            let at = *offset;
            *offset += u64::from(c.len);
            Some((at, c))
        })
    }

    /// The parent directory's path, `None` at the folder root.
    pub fn parent(&self) -> Option<&str> {
        parent_of(&self.path)
    }
}

/// How many ancestor hashes a record carries.
pub const MAX_ANCESTORS: usize = 8;

impl FileRecord {
    /// Ancestry for a new version edited from `self`: `self` first.
    pub fn ancestry_for_child(&self) -> Vec<Hash> {
        if !self.is_live_file() {
            return Vec::new();
        }
        std::iter::once(self.hash)
            .chain(self.ancestors.iter().copied())
            .take(MAX_ANCESTORS)
            .collect()
    }
}

/// The parent of a `/`-separated relative path, `None` at the root.
pub fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_and_offsets() {
        assert_eq!(parent_of("a/b/c"), Some("a/b"));
        assert_eq!(parent_of("a"), None);
        let r = FileRecord {
            path: "x/y".into(),
            kind: EntryKind::File,
            deleted: false,
            size: 5,
            mtime_ns: 0,
            version: VersionVector::new(),
            modified_by: DeviceId::from_bytes([0; 32]),
            hash: Hash::default(),
            chunks: vec![
                ChunkRef {
                    hash: Hash::default(),
                    len: 2,
                },
                ChunkRef {
                    hash: Hash::default(),
                    len: 3,
                },
            ],
            ancestors: Vec::new(),
        };
        assert_eq!(r.parent(), Some("x"));
        let offsets: Vec<u64> = r.chunk_offsets().map(|(o, _)| o).collect();
        assert_eq!(offsets, vec![0, 2]);
    }
}

/// Whether `path` is an acceptable relative path from a peer: non-empty
/// `/`-separated components, none of them `.`/`..`/empty, no backslashes or
/// NULs, and not BetterSync's own bookkeeping. Anything else could escape
/// the folder and is rejected.
pub fn is_valid_rel_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
        && path.split('/').next() != Some(".bsync")
        && !path
            .rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with(".bsync-tmp-"))
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn rejects_escaping_and_internal_paths() {
        for ok in ["a", "a/b.txt", ".hidden", "x/.bsync", "a..b"] {
            assert!(is_valid_rel_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a\\b",
            "a\0",
            ".bsync",
            ".bsync/versions/x",
            "d/.bsync-tmp-f-1",
        ] {
            assert!(!is_valid_rel_path(bad), "{bad:?}");
        }
    }
}
