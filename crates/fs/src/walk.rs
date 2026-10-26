//! Recursive directory walk over a [`FileSystem`].

use std::io;
use std::path::{Path, PathBuf};

use crate::{BSYNC_DIR, FileKind, FileSystem, Metadata, TEMP_PREFIX};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    /// Relative to the folder root, `/`-separated.
    pub path: String,
    pub metadata: Metadata,
}

#[derive(Debug, Default)]
pub struct WalkResult {
    /// Files and directories found, parents before children.
    pub entries: Vec<WalkEntry>,
    /// Directories that could not be read (relative paths). Their contents
    /// are unknown — callers must not treat them as deleted.
    pub errors: Vec<(String, io::Error)>,
    /// Entries skipped because they cannot be synced (symlinks, names that
    /// are not valid UTF-8).
    pub skipped: Vec<PathBuf>,
}

/// The absolute path of relative path `rel` under `root`.
pub fn rel_to_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// Whether `rel` is BetterSync's own bookkeeping (never synced).
pub fn is_internal(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    first == BSYNC_DIR
        || rel
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(TEMP_PREFIX))
}

/// Walk the tree under `root/start` (`start` is relative; `""` for the whole
/// folder). The starting entry itself is included unless it is the root.
/// `skip` can prune paths (e.g. ignore rules); pruned directories are not
/// descended into.
pub fn walk(
    fs: &dyn FileSystem,
    root: &Path,
    start: &str,
    skip: &dyn Fn(&str, &Metadata) -> bool,
) -> io::Result<WalkResult> {
    let mut result = WalkResult::default();
    let mut dirs = Vec::new();
    if start.is_empty() {
        dirs.push(String::new());
    } else {
        let metadata = fs.metadata(&rel_to_path(root, start))?;
        if is_internal(start) || skip(start, &metadata) {
            return Ok(result);
        }
        match metadata.kind {
            FileKind::Symlink => {
                result.skipped.push(rel_to_path(root, start));
                return Ok(result);
            }
            FileKind::Dir => dirs.push(start.to_owned()),
            FileKind::File => {}
        }
        result.entries.push(WalkEntry {
            path: start.to_owned(),
            metadata,
        });
    }
    // Depth-first with an explicit stack; children are visited in name order.
    while let Some(dir) = dirs.pop() {
        let listing = match fs.read_dir(&rel_to_path(root, &dir)) {
            Ok(l) => l,
            Err(e) => {
                result.errors.push((dir, e));
                continue;
            }
        };
        let mut subdirs = Vec::new();
        for entry in listing {
            let Some(name) = entry.name.to_str() else {
                result
                    .skipped
                    .push(rel_to_path(root, &dir).join(&entry.name));
                continue;
            };
            let rel = if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            };
            if is_internal(&rel) || skip(&rel, &entry.metadata) {
                continue;
            }
            match entry.metadata.kind {
                FileKind::Symlink => {
                    result.skipped.push(rel_to_path(root, &rel));
                    continue;
                }
                FileKind::Dir => subdirs.push(rel.clone()),
                FileKind::File => {}
            }
            result.entries.push(WalkEntry {
                path: rel,
                metadata: entry.metadata,
            });
        }
        dirs.extend(subdirs.into_iter().rev());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StdFs;

    #[test]
    fn walks_fixture_tree_skipping_internal_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = StdFs;
        for d in ["a", "a/b", ".bsync", ".bsync/versions", "c"] {
            fs.create_dir(&rel_to_path(root, d)).unwrap();
        }
        for f in ["a/1.txt", "a/b/2.txt", ".bsync/x", "c/.bsync-tmp-3", "top"] {
            fs.create_new(&rel_to_path(root, f), f.as_bytes()).unwrap();
        }

        let r = walk(&fs, root, "", &|_, _| false).unwrap();
        let paths: Vec<_> = r.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "c", "top", "a/1.txt", "a/b", "a/b/2.txt"]);
        assert!(r.errors.is_empty());

        let sub = walk(&fs, root, "a/b", &|_, _| false).unwrap();
        let paths: Vec<_> = sub.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a/b", "a/b/2.txt"]);

        let pruned = walk(&fs, root, "", &|p, _| p == "a").unwrap();
        let paths: Vec<_> = pruned.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["c", "top"]);
    }

    #[test]
    fn internal_paths() {
        assert!(is_internal(".bsync"));
        assert!(is_internal(".bsync/versions/a"));
        assert!(is_internal("x/.bsync-tmp-1"));
        assert!(!is_internal("x/.bsync"));
        assert!(!is_internal("a.bsync-tmp-"));
    }
}
