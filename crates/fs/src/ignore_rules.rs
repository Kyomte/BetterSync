//! `.bsyncignore`: gitignore-syntax patterns at the folder root. Matching
//! paths are neither scanned nor changed by sync. The file itself syncs like
//! any other, so every device ends up with the same rules.

use std::io;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::{FileSystem, rel_to_path};

pub const IGNORE_FILE: &str = ".bsyncignore";

#[derive(Debug, Clone)]
pub struct IgnoreRules {
    matcher: Gitignore,
    /// The file's content these rules were built from (to detect changes).
    source: Vec<u8>,
}

impl Default for IgnoreRules {
    fn default() -> Self {
        Self {
            matcher: Gitignore::empty(),
            source: Vec::new(),
        }
    }
}

impl IgnoreRules {
    pub fn parse(content: &[u8]) -> Self {
        let mut b = GitignoreBuilder::new("");
        for line in String::from_utf8_lossy(content).lines() {
            if let Err(e) = b.add_line(None, line) {
                tracing::warn!("{IGNORE_FILE}: ignoring bad pattern {line:?}: {e}");
            }
        }
        let matcher = b.build().unwrap_or_else(|e| {
            tracing::warn!("{IGNORE_FILE}: {e}");
            Gitignore::empty()
        });
        Self {
            matcher,
            source: content.to_vec(),
        }
    }

    /// Read `<root>/.bsyncignore` (no file means no rules).
    pub fn load(fs: &dyn FileSystem, root: &Path) -> io::Result<Self> {
        match fs.read(&rel_to_path(root, IGNORE_FILE)) {
            Ok(content) => Ok(Self::parse(&content)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Reload if the file changed. Returns whether the rules changed.
    pub fn refresh(&mut self, fs: &dyn FileSystem, root: &Path) -> io::Result<bool> {
        let fresh = Self::load(fs, root)?;
        if fresh.source == self.source {
            return Ok(false);
        }
        *self = fresh;
        Ok(true)
    }

    /// Whether relative path `rel` (or one of its parent directories) is
    /// ignored. The ignore file itself never is.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel == IGNORE_FILE || rel.is_empty() {
            return false;
        }
        self.matcher
            .matched_path_or_any_parents(Path::new(rel), is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_semantics() {
        let r = IgnoreRules::parse(b"# comment\n*.tmp\n/build/\nnode_modules\n!keep.tmp\n");
        assert!(r.is_ignored("a.tmp", false));
        assert!(r.is_ignored("deep/x/a.tmp", false));
        assert!(!r.is_ignored("keep.tmp", false));
        assert!(r.is_ignored("build", true));
        assert!(r.is_ignored("build/out.o", false));
        assert!(!r.is_ignored("src/build", true), "anchored to the root");
        assert!(r.is_ignored("x/node_modules/y/z.js", false));
        assert!(!r.is_ignored("readme.md", false));
        assert!(!r.is_ignored(IGNORE_FILE, false));
    }
}
