//! Helpers for running daemons in tests.

#![allow(clippy::unwrap_used, dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bsync_core::DeviceId;
use bsync_fs::WatchConfig;
use bsyncd::{Config, Daemon};

pub struct Dev {
    pub tmp: tempfile::TempDir,
    pub id: DeviceId,
}

impl Dev {
    pub fn new(name: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let d = Self {
            id: bsyncd::config::device_id(&tmp.path().join("config")).unwrap(),
            tmp,
        };
        let c = Config {
            device_name: name.into(),
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            ..Config::default()
        };
        c.save(&d.config_dir()).unwrap();
        std::fs::create_dir_all(d.folder()).unwrap();
        d
    }

    pub fn config_dir(&self) -> PathBuf {
        self.tmp.path().join("config")
    }

    pub fn folder(&self) -> PathBuf {
        self.tmp.path().join("folder")
    }

    pub fn edit(&self, f: impl FnOnce(&mut Config)) {
        let mut c = Config::load(&self.config_dir()).unwrap();
        f(&mut c);
        c.save(&self.config_dir()).unwrap();
    }

    pub fn start(&self) -> Daemon {
        Daemon::start_with(
            &self.config_dir(),
            WatchConfig {
                debounce: Duration::from_millis(200),
                full_rescan: Duration::from_secs(3600),
                ..WatchConfig::default()
            },
        )
        .unwrap()
    }
}

/// Relative path → content (None for directories), excluding `.bsync`.
pub fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
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
            } else if let Ok(data) = std::fs::read(e.path()) {
                out.insert(rel, Some(data));
            }
        }
    }
    out
}

/// Wait up to `secs` for `cond`.
pub fn eventually(secs: u64, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
