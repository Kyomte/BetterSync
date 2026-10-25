//! In-memory implementation of [`FileSystem`].
//!
//! Paths are reduced to their normal components, so `/a/b` and `a/b` name the
//! same entry and the root is the empty path. Modification times come from a
//! logical clock that advances on every mutation, so they are deterministic
//! and every change produces a new mtime.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use bsync_fs::{DirEntry, FileKind, FileSystem, Metadata};

type Key = Vec<String>;

#[derive(Debug, Clone)]
enum Node {
    File { data: Arc<Vec<u8>>, modified: u64 },
    Dir { modified: u64 },
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<Key, Node>,
    clock: u64,
    /// Paths whose rename/remove fails as if another process held them open.
    locked: BTreeSet<Key>,
}

/// A shareable in-memory file system. Clones share the same tree.
#[derive(Debug, Clone, Default)]
pub struct MemFs {
    inner: Arc<Mutex<Inner>>,
}

fn key(path: &Path) -> io::Result<Key> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(name) => out.push(
                name.to_str()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path"))?
                    .to_owned(),
            ),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "`..` in path"));
            }
        }
    }
    Ok(out)
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

fn tick_time(tick: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_millis(tick)
}

impl Inner {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn get(&self, k: &Key) -> Option<&Node> {
        if k.is_empty() {
            return Some(&Node::Dir { modified: 0 });
        }
        self.nodes.get(k)
    }

    fn require_parent_dir(&self, k: &Key) -> io::Result<()> {
        match k.split_last() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "operation on root",
            )),
            Some((_, parent)) => match self.get(&parent.to_vec()) {
                Some(Node::Dir { .. }) => Ok(()),
                Some(Node::File { .. }) => Err(io::Error::from(io::ErrorKind::NotADirectory)),
                None => Err(not_found()),
            },
        }
    }

    fn children(&self, k: &Key) -> impl Iterator<Item = (&Key, &Node)> {
        self.nodes
            .range(k.clone()..)
            .take_while(move |(child, _)| child.starts_with(k))
            .filter(move |(child, _)| child.len() == k.len() + 1)
    }

    fn has_descendants(&self, k: &Key) -> bool {
        self.nodes
            .range(k.clone()..)
            .any(|(child, _)| child.starts_with(k) && child.len() > k.len())
    }

    fn check_unlocked(&self, k: &Key) -> io::Result<()> {
        if self.locked.contains(k) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file is locked by another process",
            ))
        } else {
            Ok(())
        }
    }
}

fn metadata_of(node: &Node) -> Metadata {
    match node {
        Node::File { data, modified } => Metadata {
            kind: FileKind::File,
            len: data.len() as u64,
            modified: tick_time(*modified),
        },
        Node::Dir { modified } => Metadata {
            kind: FileKind::Dir,
            len: 0,
            modified: tick_time(*modified),
        },
    }
}

impl MemFs {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock already fails the test; keep going.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Simulate another process holding `path` open (Windows-style): renames
    /// and removals of it fail with `PermissionDenied` until unlocked.
    pub fn set_locked(&self, path: &Path, locked: bool) {
        let Ok(k) = key(path) else { return };
        let mut inner = self.lock();
        if locked {
            inner.locked.insert(k);
        } else {
            inner.locked.remove(&k);
        }
    }

    /// Write `data` to `path` in place, creating it if needed — what a user's
    /// editor does. The sync engine itself never does this.
    pub fn write_in_place(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let k = key(path)?;
        let mut inner = self.lock();
        inner.require_parent_dir(&k)?;
        if let Some(Node::Dir { .. }) = inner.nodes.get(&k) {
            return Err(io::Error::from(io::ErrorKind::IsADirectory));
        }
        let modified = inner.tick();
        inner.nodes.insert(
            k,
            Node::File {
                data: Arc::new(data.to_vec()),
                modified,
            },
        );
        Ok(())
    }

    /// Every file under `root` (relative path → content), for comparing trees.
    pub fn snapshot(&self, root: &Path) -> io::Result<BTreeMap<String, Option<Arc<Vec<u8>>>>> {
        let k = key(root)?;
        let inner = self.lock();
        Ok(inner
            .nodes
            .range(k.clone()..)
            .take_while(|(p, _)| p.starts_with(&k))
            .filter(|(p, _)| p.len() > k.len())
            .map(|(p, node)| {
                let rel = p[k.len()..].join("/");
                let data = match node {
                    Node::File { data, .. } => Some(Arc::clone(data)),
                    Node::Dir { .. } => None,
                };
                (rel, data)
            })
            .collect())
    }
}

impl FileSystem for MemFs {
    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let k = key(path)?;
        let inner = self.lock();
        inner.get(&k).map(metadata_of).ok_or_else(not_found)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let k = key(path)?;
        let inner = self.lock();
        match inner.get(&k) {
            Some(Node::Dir { .. }) => {}
            Some(Node::File { .. }) => return Err(io::Error::from(io::ErrorKind::NotADirectory)),
            None => return Err(not_found()),
        }
        Ok(inner
            .children(&k)
            .map(|(child, node)| DirEntry {
                name: OsString::from(child[child.len() - 1].clone()),
                metadata: metadata_of(node),
            })
            .collect())
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        let k = key(path)?;
        let inner = self.lock();
        match inner.get(&k) {
            Some(Node::File { data, .. }) => {
                Ok(Box::new(io::Cursor::new(ArcBytes(Arc::clone(data)))))
            }
            Some(Node::Dir { .. }) => Err(io::Error::from(io::ErrorKind::IsADirectory)),
            None => Err(not_found()),
        }
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let k = key(path)?;
        let inner = self.lock();
        match inner.get(&k) {
            Some(Node::File { data, .. }) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                let end = start.saturating_add(len).min(data.len());
                Ok(data[start..end].to_vec())
            }
            Some(Node::Dir { .. }) => Err(io::Error::from(io::ErrorKind::IsADirectory)),
            None => Err(not_found()),
        }
    }

    fn create_new_with(
        &self,
        path: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        let k = key(path)?;
        {
            let inner = self.lock();
            inner.require_parent_dir(&k)?;
            if inner.get(&k).is_some() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
        }
        // Fill the buffer without holding the lock: `write` may read this FS.
        let mut data = Vec::new();
        write(&mut data)?;
        let mut inner = self.lock();
        inner.require_parent_dir(&k)?;
        if inner.get(&k).is_some() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let modified = inner.tick();
        inner.nodes.insert(
            k,
            Node::File {
                data: Arc::new(data),
                modified,
            },
        );
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from = key(from)?;
        let to = key(to)?;
        let mut inner = self.lock();
        let node = inner.get(&from).cloned().ok_or_else(not_found)?;
        if from.is_empty() || to.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rename of root",
            ));
        }
        inner.check_unlocked(&from)?;
        inner.check_unlocked(&to)?;
        inner.require_parent_dir(&to)?;
        if from == to {
            return Ok(());
        }
        match (&node, inner.get(&to)) {
            (_, None) => {}
            (Node::File { .. }, Some(Node::File { .. })) => {}
            (Node::File { .. }, Some(Node::Dir { .. })) => {
                return Err(io::Error::from(io::ErrorKind::IsADirectory));
            }
            (Node::Dir { .. }, Some(Node::File { .. })) => {
                return Err(io::Error::from(io::ErrorKind::NotADirectory));
            }
            (Node::Dir { .. }, Some(Node::Dir { .. })) => {
                if inner.has_descendants(&to) {
                    return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
                }
            }
        }
        if to.starts_with(&from) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot move a directory into itself",
            ));
        }
        // Move the entry and its whole subtree.
        let moved: Vec<Key> = inner
            .nodes
            .range(from.clone()..)
            .take_while(|(p, _)| p.starts_with(&from))
            .map(|(p, _)| p.clone())
            .collect();
        for old in moved {
            if let Some(n) = inner.nodes.remove(&old) {
                let mut new = to.clone();
                new.extend_from_slice(&old[from.len()..]);
                inner.nodes.insert(new, n);
            }
        }
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let k = key(path)?;
        let mut inner = self.lock();
        match inner.get(&k) {
            Some(Node::File { .. }) => {}
            Some(Node::Dir { .. }) => return Err(io::Error::from(io::ErrorKind::IsADirectory)),
            None => return Err(not_found()),
        }
        inner.check_unlocked(&k)?;
        inner.nodes.remove(&k);
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let k = key(path)?;
        let mut inner = self.lock();
        inner.require_parent_dir(&k)?;
        if inner.get(&k).is_some() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let modified = inner.tick();
        inner.nodes.insert(k, Node::Dir { modified });
        Ok(())
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let k = key(path)?;
        let mut inner = self.lock();
        match inner.get(&k) {
            Some(Node::Dir { .. }) if !k.is_empty() => {}
            Some(Node::Dir { .. }) => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "remove root"));
            }
            Some(Node::File { .. }) => return Err(io::Error::from(io::ErrorKind::NotADirectory)),
            None => return Err(not_found()),
        }
        if inner.has_descendants(&k) {
            return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty));
        }
        inner.nodes.remove(&k);
        Ok(())
    }
}

/// Lets a `Cursor` read shared file content without copying it.
struct ArcBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_operations() {
        let fs = MemFs::new();
        fs.create_dir(Path::new("/d")).unwrap();
        fs.create_new(Path::new("/d/a"), b"hello").unwrap();
        assert_eq!(
            fs.create_new(Path::new("/d/a"), b"x").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs.create_new(Path::new("/nope/a"), b"x")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs.read(Path::new("d/a")).unwrap(), b"hello");
        assert_eq!(fs.read_at(Path::new("/d/a"), 3, 10).unwrap(), b"lo");

        let m1 = fs.metadata(Path::new("/d/a")).unwrap();
        fs.write_in_place(Path::new("/d/a"), b"hellp").unwrap();
        let m2 = fs.metadata(Path::new("/d/a")).unwrap();
        assert_eq!(m1.len, m2.len);
        assert!(m2.modified > m1.modified, "every write gets a new mtime");

        assert_eq!(
            fs.remove_dir(Path::new("/d")).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        fs.rename(Path::new("/d"), Path::new("/e")).unwrap();
        assert_eq!(fs.read(Path::new("/e/a")).unwrap(), b"hellp");
        let names: Vec<_> = fs
            .read_dir(Path::new("/"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec![OsString::from("e")]);

        fs.set_locked(Path::new("/e/a"), true);
        assert_eq!(
            fs.remove_file(Path::new("/e/a")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs.set_locked(Path::new("/e/a"), false);
        fs.remove_file(Path::new("/e/a")).unwrap();
        fs.remove_dir(Path::new("/e")).unwrap();
        assert!(fs.snapshot(Path::new("/")).unwrap().is_empty());
    }

    #[test]
    fn rename_replaces_files_but_not_dirs() {
        let fs = MemFs::new();
        fs.create_new(Path::new("/a"), b"new").unwrap();
        fs.create_new(Path::new("/b"), b"old").unwrap();
        fs.rename(Path::new("/a"), Path::new("/b")).unwrap();
        assert_eq!(fs.read(Path::new("/b")).unwrap(), b"new");
        fs.create_dir(Path::new("/d")).unwrap();
        assert!(fs.rename(Path::new("/b"), Path::new("/d")).is_err());
        assert!(fs.rename(Path::new("/d"), Path::new("/d/x")).is_err());
    }
}
