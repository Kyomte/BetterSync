//! Explorer sync badges on Windows (stretch goal S1).
//!
//! A synced folder is registered as a Cloud Files *sync root*. Its files
//! are converted to **full** placeholders: the content stays exactly where
//! it is, and Explorer shows a green tick for "in sync" and the sync arrows
//! for "not in sync". Files are never dehydrated (online-only): the root's
//! hydration policy makes Windows refuse it, and files are pinned
//! ("always keep on this device"), because the daemon cannot serve
//! content on demand.
//!
//! On other platforms everything here is a no-op that reports
//! [`Error::Unsupported`].

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Explorer integration is only available on Windows")]
    Unsupported,
    #[error("{what} {path}: {message}")]
    Os {
        what: &'static str,
        path: String,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::SyncRoot;

/// A registered sync root (stub: never constructed off Windows).
#[cfg(not(windows))]
#[derive(Debug)]
pub struct SyncRoot(());

#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(windows))]
impl SyncRoot {
    pub fn register(_root: &Path, _folder_id: &str) -> Result<Self> {
        Err(Error::Unsupported)
    }

    pub fn set_in_sync(&self, _path: &Path, _in_sync: bool) -> Result<bool> {
        Err(Error::Unsupported)
    }

    pub fn unregister(_root: &Path) -> Result<()> {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn unsupported_off_windows() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            SyncRoot::register(dir.path(), "x"),
            Err(Error::Unsupported)
        ));
    }

    /// Needs the Cloud Files filter driver (cldflt), present on Windows 10
    /// 1709+; set `BSYNC_SKIP_CLOUDFILES=1` where it is unavailable.
    #[cfg(windows)]
    #[test]
    fn badges_follow_set_in_sync() {
        if std::env::var_os("BSYNC_SKIP_CLOUDFILES").is_some() {
            eprintln!("skipped: BSYNC_SKIP_CLOUDFILES is set");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).expect("mkdir");
        let file = root.join("a.txt");
        std::fs::write(&file, b"content").expect("write");
        let sync_root = SyncRoot::register(&root, "test-folder").expect("register sync root");
        assert!(sync_root.set_in_sync(&file, true).expect("mark in sync"));
        assert!(!sync_root.set_in_sync(&file, true).expect("no-op"));
        assert!(
            sync_root
                .set_in_sync(&file, false)
                .expect("mark not in sync")
        );
        // Content and metadata untouched.
        assert_eq!(std::fs::read(&file).expect("read"), b"content");
        std::fs::write(&file, b"edited").expect("edit placeholder");
        assert_eq!(std::fs::read(&file).expect("read"), b"edited");
        drop(sync_root);
        SyncRoot::unregister(&root).expect("unregister");
        assert_eq!(std::fs::read(&file).expect("read"), b"edited");
    }
}
