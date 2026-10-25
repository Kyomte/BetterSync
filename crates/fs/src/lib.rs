//! File-system layer for BetterSync.
//!
//! All file access goes through the [`FileSystem`] trait so the simulator can
//! swap in an in-memory implementation. [`StdFs`] is the real one.
//!
//! The trait deliberately has no way to open an existing file for writing:
//! new content is always written to a fresh file with [`FileSystem::create_new`]
//! and moved into place with [`FileSystem::rename`] (invariant 1).

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::SystemTime;

mod walk;

pub use walk::{WalkEntry, WalkResult, is_internal, rel_to_path, walk};

/// BetterSync's own directory at the root of every synced folder.
pub const BSYNC_DIR: &str = ".bsync";

/// Prefix of temporary files written next to their final location.
pub const TEMP_PREFIX: &str = ".bsync-tmp-";

/// Nanoseconds since the Unix epoch (negative before it).
pub fn mtime_ns(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

/// What kind of entry a path refers to. Symlinks are not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub kind: FileKind,
    /// Length in bytes; 0 for directories.
    pub len: u64,
    /// Only a hint for "did this change since the last scan" — never used
    /// for ordering (invariant 5).
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: OsString,
    pub metadata: Metadata,
}

pub trait FileSystem: Send + Sync {
    /// Metadata of `path`, without following symlinks.
    fn metadata(&self, path: &Path) -> io::Result<Metadata>;

    /// Entries of directory `path`, sorted by name.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;

    /// Open the file at `path` for sequential reading.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;

    /// Read up to `len` bytes starting at `offset`. Returns fewer bytes only
    /// at end of file.
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Create a file that must not exist yet, let `write` fill it, then flush
    /// it to stable storage. Fails with `AlreadyExists` if `path` exists.
    fn create_new_with(
        &self,
        path: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()>;

    /// Atomically move `from` to `to`, replacing `to` if it is a file.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Create one directory; its parent must exist.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Remove an empty directory.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;

    /// Whole content of the file at `path`.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        self.open_read(path)?.read_to_end(&mut data)?;
        Ok(data)
    }

    /// [`FileSystem::create_new_with`] for content already in memory.
    fn create_new(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.create_new_with(path, &mut |w| w.write_all(data))
    }
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

impl FileSystem for StdFs {
    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        convert_metadata(&fs::symlink_metadata(path)?)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut entries = fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                Ok(DirEntry {
                    name: entry.file_name(),
                    metadata: convert_metadata(&fs::symlink_metadata(entry.path())?)?,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        Ok(Box::new(io::BufReader::new(fs::File::open(path)?)))
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::with_capacity(len);
        file.take(len as u64).read_to_end(&mut data)?;
        Ok(data)
    }

    fn create_new_with(
        &self,
        path: &Path,
        write: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut writer = io::BufWriter::new(file);
        write(&mut writer)?;
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(path)
    }
}

fn convert_metadata(meta: &fs::Metadata) -> io::Result<Metadata> {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else {
        FileKind::File
    };
    Ok(Metadata {
        kind,
        len: if kind == FileKind::Dir { 0 } else { meta.len() },
        modified: meta.modified()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_fs_create_rename_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let tmp = dir.path().join("a.tmp");
        let dst = dir.path().join("a.txt");

        fs.create_new(&tmp, b"hello").unwrap();
        assert_eq!(
            fs.create_new(&tmp, b"again").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs.create_new(&dst, b"old").unwrap();
        fs.rename(&tmp, &dst).unwrap();
        assert_eq!(fs.read(&dst).unwrap(), b"hello");
        assert_eq!(fs.read_at(&dst, 1, 3).unwrap(), b"ell");
        assert_eq!(fs.read_at(&dst, 3, 10).unwrap(), b"lo");

        fs.create_dir(&dir.path().join("sub")).unwrap();
        let names: Vec<_> = fs
            .read_dir(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.metadata.kind, e.metadata.len))
            .collect();
        assert_eq!(
            names,
            vec![
                ("a.txt".into(), FileKind::File, 5),
                ("sub".into(), FileKind::Dir, 0),
            ]
        );

        fs.remove_file(&dst).unwrap();
        fs.remove_dir(&dir.path().join("sub")).unwrap();
        assert!(fs.read_dir(dir.path()).unwrap().is_empty());
    }
}
