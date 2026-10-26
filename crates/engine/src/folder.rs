//! One synced folder on this device: its index, scanning, and applying
//! planned actions to disk.
//!
//! Applying follows the invariants in PLAN.md:
//! 1. New content goes to a temp file in the same directory, is flushed,
//!    verified against the expected hash and only then renamed into place.
//! 2. Content being replaced is copied (or, for deletions, moved) into
//!    `.bsync/versions/` first.
//! 3. Right before replacing or deleting, the local file is re-hashed; if it
//!    no longer matches the index, the action is abandoned so the local
//!    change can be scanned and re-planned.
//! 4. A version that loses a conflict is kept as a conflict copy.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bsync_core::names::{to_nfc, windows_path_problem};
use bsync_core::plan::{Action, apply_order, conflict_copy_path, plan_path};
use bsync_core::{
    ChunkParams, ChunkRef, DeviceId, EntryKind, FileRecord, Hash, chunk_reader, format_utc_compact,
};
use bsync_fs::{
    BSYNC_DIR, FileKind, FileSystem, IGNORE_FILE, IgnoreRules, Metadata, TEMP_PREFIX, mtime_ns,
    rel_to_path,
};
use bsync_index::{Index, LocalEntry};

use crate::scan::{ScanContext, ScanStats, new_version, stat_of};
use crate::{EngineError, Result};

/// Source of the current time, injectable for the simulator. Only used to
/// name files in the versions folder — never to order versions.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Where chunks that are not in local files come from (a peer, or in tests
/// another folder).
pub trait ChunkSource {
    fn has(&self, hash: &Hash) -> bool;
    fn get(&mut self, hash: &Hash) -> Option<Vec<u8>>;
}

/// A [`ChunkSource`] with nothing in it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChunks;

impl ChunkSource for NoChunks {
    fn has(&self, _: &Hash) -> bool {
        false
    }
    fn get(&mut self, _: &Hash) -> Option<Vec<u8>> {
        None
    }
}

/// Result of trying to apply one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The disk no longer matches the index at this path; rescan it and
    /// plan again.
    LocalChanged,
    /// These chunks must be fetched first.
    MissingChunks(Vec<Hash>),
    /// The action no longer matches the plan for this path; plan again.
    Stale,
    /// Cannot be applied right now (e.g. the parent is a file, or the name
    /// is not allowed here); retried later.
    Blocked(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncStats {
    pub applied: u64,
    /// Chunks copied out of local files instead of being fetched.
    pub chunks_reused: u64,
    pub chunks_fetched: u64,
    pub conflicts: u64,
    pub blocked: Vec<(String, String)>,
}

pub type SkipFn = dyn Fn(&str, &Metadata) -> bool + Send + Sync;

pub struct Folder {
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    root: PathBuf,
    device: DeviceId,
    params: ChunkParams,
    index: Index,
    skip: Box<SkipFn>,
    temp_counter: u64,
    /// `.bsyncignore` rules, reloaded on every scan.
    ignore: IgnoreRules,
    ignore_changed: bool,
    /// Refuse names Windows cannot store (on by default on Windows).
    windows_rules: bool,
    /// Record path (NFC) → on-disk path, where they differ.
    disk_names: BTreeMap<String, String>,
}

impl std::fmt::Debug for Folder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Folder")
            .field("root", &self.root)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

fn action_name(a: &Action) -> &'static str {
    match a {
        Action::Apply(_) => "apply",
        Action::AdoptVersion(_) => "adopt",
        Action::Conflict { .. } => "conflict",
    }
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// Split `name` into stem and extension (`.ext` or empty).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl Folder {
    pub fn new(
        fs: Arc<dyn FileSystem>,
        clock: Arc<dyn Clock>,
        root: PathBuf,
        device: DeviceId,
        params: ChunkParams,
        index: Index,
    ) -> Self {
        Self {
            fs,
            clock,
            root,
            device,
            params,
            index,
            skip: Box::new(|_, _| false),
            temp_counter: 0,
            ignore: IgnoreRules::default(),
            ignore_changed: false,
            windows_rules: cfg!(windows),
            disk_names: BTreeMap::new(),
        }
    }

    /// Apply Windows' file-name rules to incoming paths (on by default only
    /// on Windows; tests turn it on elsewhere).
    pub fn set_windows_rules(&mut self, on: bool) {
        self.windows_rules = on;
    }

    /// Reload `.bsyncignore` if it changed. Returns whether it did; the
    /// caller must then re-plan everything and rescan, since paths may have
    /// become (un)ignored.
    pub fn refresh_ignore(&mut self) -> Result<bool> {
        let changed = self.ignore.refresh(&*self.fs, &self.root)?;
        if changed {
            tracing::info!(root = %self.root.display(), "{IGNORE_FILE} changed");
            self.ignore_changed = true;
        }
        Ok(changed)
    }

    /// Whether the rules changed since the last call.
    pub fn take_ignore_changed(&mut self) -> bool {
        std::mem::take(&mut self.ignore_changed)
    }

    /// Whether `.bsyncignore` excludes `rel` (a record path).
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        self.ignore.is_ignored(rel, is_dir)
    }

    /// Leave out paths for which `skip` returns true (ignore rules).
    pub fn set_skip(&mut self, skip: Box<SkipFn>) {
        self.skip = skip;
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn index_mut(&mut self) -> &mut Index {
        &mut self.index
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    pub fn fs(&self) -> &dyn FileSystem {
        &*self.fs
    }

    pub fn fs_arc(&self) -> Arc<dyn FileSystem> {
        Arc::clone(&self.fs)
    }

    pub fn params(&self) -> ChunkParams {
        self.params
    }

    /// The current Unix time in seconds: the lowest counter a new local
    /// version gets (see [`bsync_core::VersionVector::incremented_at_least`]).
    fn counter_floor(&self) -> u64 {
        self.clock
            .now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// A fresh counter for a new local version.
    fn own_counter(&self) -> Result<u64> {
        Ok(self.index.next_own_counter(self.counter_floor())?)
    }

    /// Absolute on-disk path of record path `rel`. Usually the same name;
    /// differs where the disk holds a non-NFC form of it (or of a parent).
    pub fn abs(&self, rel: &str) -> PathBuf {
        rel_to_path(&self.root, &self.disk_rel(rel))
    }

    fn disk_rel(&self, rel: &str) -> String {
        if self.disk_names.is_empty() {
            return rel.to_owned();
        }
        if let Some(d) = self.disk_names.get(rel) {
            return d.clone();
        }
        // A new entry inside a directory whose disk name differs.
        let mut cur = rel;
        while let Some(parent) = bsync_core::parent_of(cur) {
            if let Some(d) = self.disk_names.get(parent) {
                return format!("{d}{}", &rel[parent.len()..]);
            }
            cur = parent;
        }
        rel.to_owned()
    }

    /// Whether `rel` (a record path) is excluded from syncing.
    pub fn is_skipped(&self, rel: &str, meta: &Metadata) -> bool {
        self.ignore.is_ignored(rel, meta.kind == FileKind::Dir) || (self.skip)(rel, meta)
    }

    /// Scan `subtree` (`""` = everything) for local changes. `subtree` may
    /// be a record path or an on-disk path (as file watchers report them).
    pub fn scan(&mut self, subtree: &str) -> Result<ScanStats> {
        self.refresh_ignore()?;
        let disk_subtree = self
            .disk_names
            .get(subtree)
            .cloned()
            .unwrap_or_else(|| subtree.to_owned());
        let ignore = &self.ignore;
        let extra = &self.skip;
        let skip = |rel: &str, meta: &Metadata| {
            ignore.is_ignored(&to_nfc(rel), meta.kind == FileKind::Dir) || extra(rel, meta)
        };
        let ctx = ScanContext {
            fs: &*self.fs,
            root: &self.root,
            device: self.device,
            counter_floor: self.counter_floor(),
            params: self.params,
            skip: &skip,
        };
        let stats = crate::scan::scan(&ctx, &mut self.index, &disk_subtree)?;
        let nfc_subtree = to_nfc(subtree);
        self.disk_names.retain(|k, _| {
            !(nfc_subtree.is_empty()
                || *k == nfc_subtree
                || k.starts_with(&format!("{nfc_subtree}/")))
        });
        self.disk_names.extend(stats.disk_names.iter().cloned());
        Ok(stats)
    }

    /// Why an incoming entry at `path` cannot be created here, if it can't:
    /// names Windows cannot store, and names that only differ in case or
    /// Unicode form from an existing local entry (or one of its parents) —
    /// on a case-insensitive file system they would be the same file.
    fn name_problem(&self, path: &str) -> Result<Option<String>> {
        if self.windows_rules
            && let Some(why) = windows_path_problem(path)
        {
            return Ok(Some(why));
        }
        let mut prefixes = vec![path];
        let mut cur = path;
        while let Some(p) = bsync_core::parent_of(cur) {
            prefixes.push(p);
            cur = p;
        }
        for prefix in prefixes {
            for other in self.index.same_case_key(prefix)? {
                if other.record.path != prefix {
                    return Ok(Some(format!(
                        "{prefix:?} differs from existing {:?} only in case or Unicode form",
                        other.record.path
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Remove old versions from `.bsync/versions/`: anything older than
    /// `max_age`, then the oldest until at most `max_bytes` remain. Returns
    /// how many files were removed.
    pub fn clean_versions(&self, max_age: std::time::Duration, max_bytes: u64) -> Result<usize> {
        let root = format!("{BSYNC_DIR}/versions");
        let mut files = Vec::new(); // (timestamp, size, rel)
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            let listing = match self.fs.read_dir(&rel_to_path(&self.root, &dir)) {
                Ok(l) => l,
                Err(e) if not_found(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            for e in listing {
                let name = e.name.to_string_lossy().into_owned();
                let rel = format!("{dir}/{name}");
                match e.metadata.kind {
                    FileKind::Dir => dirs.push(rel),
                    FileKind::File => {
                        // The archive time is in the name (`stem~YYYYMMDD-HHMMSS`),
                        // so this works the same on every file system.
                        let stamp = name
                            .rsplit_once('~')
                            .map(|(_, rest)| rest.get(..15).unwrap_or(rest).to_owned())
                            .unwrap_or_default();
                        files.push((stamp, e.metadata.len, rel));
                    }
                    FileKind::Symlink => {}
                }
            }
        }
        let cutoff = format_utc_compact(mtime_ns(
            self.clock
                .now()
                .checked_sub(max_age)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        ));
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        let mut removed = 0;
        for (stamp, size, rel) in files {
            if stamp.as_str() >= cutoff.as_str() && total <= max_bytes {
                break;
            }
            match self.fs.remove_file(&rel_to_path(&self.root, &rel)) {
                Ok(()) => {
                    total -= size;
                    removed += 1;
                }
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(removed)
    }

    // ----- planning -----

    /// The planned action for one path, given what peers announced.
    pub fn plan(&self, path: &str) -> Result<Option<Action>> {
        let local = self.index.get(path)?.map(|e| e.record);
        let remotes: Vec<FileRecord> = self
            .index
            .remote(path)?
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        let is_dir = local
            .iter()
            .chain(&remotes)
            .any(|r| r.kind == EntryKind::Dir);
        if self.ignore.is_ignored(path, is_dir) {
            return Ok(None);
        }
        Ok(plan_path(local.as_ref(), &remotes))
    }

    /// Planned actions for every path any peer announced, in apply order.
    pub fn plan_all(&self) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        for path in self.index.remote_paths()? {
            if let Some(a) = self.plan(&path)? {
                actions.push(a);
            }
        }
        actions.sort_by(apply_order);
        Ok(actions)
    }

    // ----- chunks -----

    /// Read chunk `hash` from a local file that the index says contains it,
    /// verifying the content.
    pub fn local_chunk(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        for loc in self.index.find_chunk(hash)? {
            match self
                .fs
                .read_at(&self.abs(&loc.path), loc.offset, loc.len as usize)
            {
                Ok(data) if Hash::of(&data) == *hash => return Ok(Some(data)),
                Ok(_) => {}
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    // ----- applying -----

    /// Apply one planned action.
    pub fn apply(
        &mut self,
        action: &Action,
        source: &mut dyn ChunkSource,
        stats: &mut SyncStats,
    ) -> Result<Outcome> {
        let path = action.record().path.clone();
        // Actions are planned in batches; an earlier one in the batch (e.g.
        // saving a conflict copy) may have changed this path since.
        if self.plan(&path)?.as_ref() != Some(action) {
            return Ok(Outcome::Stale);
        }
        let entry = self.index.get(&path)?;
        let outcome = match action {
            Action::AdoptVersion(record) => {
                self.index.put(record, entry.and_then(|e| e.stat))?;
                Outcome::Done
            }
            Action::Apply(record) | Action::Conflict { record, .. } => {
                let loser = match action {
                    Action::Conflict { loser, .. } => Some(loser),
                    _ => None,
                };
                if let Some(loser) = loser {
                    // The plan was made against this local record; if it has
                    // moved on, plan again.
                    if entry.as_ref().map(|e| &e.record) != Some(loser) {
                        return Ok(Outcome::LocalChanged);
                    }
                }
                let creating = !record.deleted && entry.as_ref().is_none_or(|e| e.record.deleted);
                if creating && let Some(why) = self.name_problem(&path)? {
                    return Ok(Outcome::Blocked(why));
                }
                if record.deleted {
                    self.apply_delete(record, entry.as_ref())?
                } else if record.kind == EntryKind::Dir {
                    self.apply_dir(record, entry.as_ref(), loser.is_some())?
                } else {
                    self.apply_file(record, entry.as_ref(), loser.is_some(), source, stats)?
                }
            }
        };
        tracing::debug!(
            device = ?self.device,
            path = %path,
            action = action_name(action),
            deleted = action.record().deleted,
            version = ?action.record().version,
            hash = ?action.record().hash,
            ?outcome,
            "apply"
        );
        if outcome == Outcome::Done {
            stats.applied += 1;
            if matches!(action, Action::Conflict { .. }) {
                stats.conflicts += 1;
            }
        }
        Ok(outcome)
    }

    /// What is on disk at `rel`, if anything.
    fn disk(&self, rel: &str) -> Result<Option<Metadata>> {
        match self.fs.metadata(&self.abs(rel)) {
            Ok(m) => Ok(Some(m)),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Invariant 3: does the disk still hold exactly what the index says?
    /// Files are re-hashed, not just stat'ed.
    fn disk_matches(&self, rel: &str, entry: Option<&LocalEntry>) -> Result<bool> {
        let expected = entry.map(|e| &e.record).filter(|r| !r.deleted);
        let Some(meta) = self.disk(rel)? else {
            return Ok(expected.is_none());
        };
        Ok(match (meta.kind, expected) {
            (FileKind::Dir, Some(r)) => r.kind == EntryKind::Dir,
            (FileKind::File, Some(r)) if r.kind == EntryKind::File => {
                if meta.len != r.size {
                    return Ok(false);
                }
                let reader = match self.fs.open_read(&self.abs(rel)) {
                    Ok(r) => r,
                    Err(e) if not_found(&e) => return Ok(false),
                    Err(e) => return Err(e.into()),
                };
                chunk_reader(reader, self.params)?.hash == r.hash
            }
            _ => false,
        })
    }

    fn create_dir_all(&self, abs: &Path) -> Result<()> {
        match self.fs.metadata(abs) {
            Ok(m) if m.kind == FileKind::Dir => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a directory", abs.display()),
                )
                .into());
            }
            Err(e) if not_found(&e) => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = abs.parent() {
            self.create_dir_all(parent)?;
        }
        match self.fs.create_dir(abs) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Make sure every parent directory of `rel` exists on disk and as a
    /// live record. A parent that was deleted here is recreated as a new
    /// local version, so the deletion does not win over content inside it.
    fn ensure_parents(&mut self, rel: &str) -> Result<Option<Outcome>> {
        let mut parents = Vec::new();
        let mut cur = rel;
        while let Some(p) = bsync_core::parent_of(cur) {
            parents.push(p.to_owned());
            cur = p;
        }
        for dir in parents.into_iter().rev() {
            let entry = self.index.get(&dir)?;
            match self.disk(&dir)? {
                Some(m) if m.kind == FileKind::Dir => {}
                Some(_) => return Ok(Some(Outcome::Blocked(format!("{dir} is not a directory")))),
                None => {
                    if entry.as_ref().is_some_and(|e| !e.record.deleted) {
                        // Deleted locally but not scanned yet.
                        return Ok(Some(Outcome::LocalChanged));
                    }
                    self.fs.create_dir(&self.abs(&dir))?;
                }
            }
            if !entry.as_ref().is_some_and(|e| e.record.is_live_dir()) {
                let mut r = new_version(
                    entry.as_ref().map(|e| &e.record),
                    &dir,
                    self.device,
                    self.own_counter()?,
                );
                r.kind = EntryKind::Dir;
                self.index.put(&r, None)?;
            }
        }
        Ok(None)
    }

    fn next_temp_name(&mut self, rel: &str) -> String {
        let (dir, name) = split_dir(rel);
        self.temp_counter += 1;
        let tmp = format!("{TEMP_PREFIX}{name}-{}", self.temp_counter);
        if dir.is_empty() {
            tmp
        } else {
            format!("{dir}/{tmp}")
        }
    }

    /// A free path in `.bsync/versions/` for an old version of `rel`.
    fn versions_path(&self, rel: &str) -> Result<String> {
        let (dir, name) = split_dir(rel);
        let (stem, ext) = split_ext(name);
        let stamp = format_utc_compact(mtime_ns(self.clock.now()));
        let base = if dir.is_empty() {
            format!("{BSYNC_DIR}/versions")
        } else {
            format!("{BSYNC_DIR}/versions/{dir}")
        };
        self.create_dir_all(&self.abs(&base))?;
        for n in 0u32.. {
            let suffix = if n == 0 {
                String::new()
            } else {
                format!("-{n}")
            };
            let candidate = format!("{base}/{stem}~{stamp}{suffix}{ext}");
            if self.disk(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        unreachable!("u32 range exhausted")
    }

    /// Copy the file at `rel` into the versions folder (before replacing it).
    fn archive_copy(&mut self, rel: &str) -> Result<()> {
        let dest = self.versions_path(rel)?;
        let mut reader = self.fs.open_read(&self.abs(rel))?;
        self.fs.create_new_with(&self.abs(&dest), &mut |w| {
            io::copy(&mut reader, w).map(|_| ())
        })?;
        Ok(())
    }

    /// Move the file at `rel` into the versions folder (deleting it).
    fn archive_move(&mut self, rel: &str) -> Result<()> {
        let dest = self.versions_path(rel)?;
        self.fs.rename(&self.abs(rel), &self.abs(&dest))?;
        Ok(())
    }

    /// Move the local loser at `rel` (described by `loser`) to its conflict
    /// copy name and record that as a new local file.
    fn save_conflict_copy(&mut self, loser: &FileRecord) -> Result<()> {
        let base = conflict_copy_path(loser);
        let mut copy = base.clone();
        let mut n = 1;
        // The deterministic name may be taken by something else.
        loop {
            let taken_on_disk = self.disk(&copy)?.is_some();
            let taken_in_index = self
                .index
                .get(&copy)?
                .is_some_and(|e| !e.record.deleted && e.record.hash != loser.hash);
            if !taken_on_disk && !taken_in_index {
                break;
            }
            let (dir, name) = split_dir(&base);
            let (stem, ext) = split_ext(name);
            copy = if dir.is_empty() {
                format!("{stem}-{n}{ext}")
            } else {
                format!("{dir}/{stem}-{n}{ext}")
            };
            n += 1;
        }
        self.fs.rename(&self.abs(&loser.path), &self.abs(&copy))?;
        let meta = self.fs.metadata(&self.abs(&copy))?;
        let prev = self.index.get(&copy)?;
        let mut r = new_version(
            prev.as_ref().map(|e| &e.record),
            &copy,
            self.device,
            self.own_counter()?,
        );
        r.kind = EntryKind::File;
        r.size = loser.size;
        r.hash = loser.hash;
        r.chunks = loser.chunks.clone();
        r.mtime_ns = mtime_ns(meta.modified);
        self.index.put(&r, Some(stat_of(&meta)))?;
        tracing::debug!(device = ?self.device, path = %copy, version = ?r.version, hash = ?r.hash, "conflict copy saved");
        tracing::info!(path = %loser.path, copy = %copy, "conflict: kept local version as copy");
        Ok(())
    }

    fn apply_delete(&mut self, record: &FileRecord, entry: Option<&LocalEntry>) -> Result<Outcome> {
        let rel = record.path.as_str();
        match self.disk(rel)? {
            None => {}
            Some(m) if m.kind == FileKind::Dir => {
                let children = self.fs.read_dir(&self.abs(rel))?;
                if !children.is_empty() {
                    for child in &children {
                        let name = child.name.to_string_lossy();
                        let child_rel = format!("{rel}/{name}");
                        let known = self
                            .index
                            .get(&child_rel)?
                            .is_some_and(|e| !e.record.deleted);
                        if !known && !self.is_skipped(&child_rel, &child.metadata) {
                            return Ok(Outcome::LocalChanged);
                        }
                    }
                    // Something inside must survive (e.g. a file added here
                    // after the remote deleted the directory): keep the
                    // directory, as a version that has seen the deletion.
                    let counter = self.own_counter()?;
                    let mut r = new_version(entry.map(|e| &e.record), rel, self.device, counter);
                    r.kind = EntryKind::Dir;
                    r.version = record
                        .version
                        .merged(&entry.map(|e| e.record.version.clone()).unwrap_or_default())
                        .incremented_at_least(self.device, counter);
                    self.index.put(&r, None)?;
                    return Ok(Outcome::Done);
                }
                if !entry.is_some_and(|e| e.record.is_live_dir()) {
                    return Ok(Outcome::LocalChanged);
                }
                self.fs.remove_dir(&self.abs(rel))?;
            }
            Some(_) => {
                if !self.disk_matches(rel, entry)? {
                    return Ok(Outcome::LocalChanged);
                }
                self.archive_move(rel)?;
            }
        }
        self.index.put(record, None)?;
        Ok(Outcome::Done)
    }

    fn apply_dir(
        &mut self,
        record: &FileRecord,
        entry: Option<&LocalEntry>,
        conflict: bool,
    ) -> Result<Outcome> {
        let rel = record.path.as_str();
        match self.disk(rel)? {
            Some(m) if m.kind == FileKind::Dir => {}
            Some(_) => {
                if !self.disk_matches(rel, entry)? {
                    return Ok(Outcome::LocalChanged);
                }
                if let Some(o) = self.ensure_parents(rel)? {
                    return Ok(o);
                }
                match (conflict, entry) {
                    (true, Some(e)) => self.save_conflict_copy(&e.record.clone())?,
                    _ => self.archive_move(rel)?,
                }
                self.fs.create_dir(&self.abs(rel))?;
            }
            None => {
                if entry.is_some_and(|e| !e.record.deleted) {
                    return Ok(Outcome::LocalChanged);
                }
                if let Some(o) = self.ensure_parents(rel)? {
                    return Ok(o);
                }
                self.fs.create_dir(&self.abs(rel))?;
            }
        }
        self.index.put(record, None)?;
        Ok(Outcome::Done)
    }

    fn apply_file(
        &mut self,
        record: &FileRecord,
        entry: Option<&LocalEntry>,
        conflict: bool,
        source: &mut dyn ChunkSource,
        stats: &mut SyncStats,
    ) -> Result<Outcome> {
        let rel = record.path.as_str();

        // Everything must be available before we touch the disk.
        let mut missing = Vec::new();
        for c in &record.chunks {
            if !missing.contains(&c.hash)
                && self.index.find_chunk(&c.hash)?.is_empty()
                && !source.has(&c.hash)
            {
                missing.push(c.hash);
            }
        }
        if !missing.is_empty() {
            return Ok(Outcome::MissingChunks(missing));
        }

        if let Some(o) = self.ensure_parents(rel)? {
            return Ok(o);
        }
        if let Some(m) = self.disk(rel)?
            && m.kind == FileKind::Dir
        {
            // A directory is being replaced by a file. Its contents are
            // deleted by their own (later-ordered) actions; once it is
            // empty it can go.
            if !entry.is_some_and(|e| e.record.is_live_dir()) {
                return Ok(Outcome::LocalChanged);
            }
            if !self.fs.read_dir(&self.abs(rel))?.is_empty() {
                return Ok(Outcome::Blocked(format!("{rel} is a non-empty directory")));
            }
            self.fs.remove_dir(&self.abs(rel))?;
            // Nothing of the directory is left to protect; the record
            // below replaces its entry.
            return self.apply_file(record, None, false, source, stats);
        }

        // 1. Assemble into a temp file next to the target, verifying chunks.
        let tmp = self.next_temp_name(rel);
        let assembled = self.write_temp(&tmp, record, source, stats);
        let cleanup = |this: &Self| {
            let _ = this.fs.remove_file(&this.abs(&tmp));
        };
        match assembled {
            Ok(None) => {}
            Ok(Some(outcome)) => {
                cleanup(self);
                return Ok(outcome);
            }
            Err(e) => {
                cleanup(self);
                return Err(e);
            }
        }

        // 2. Re-check the local file right before replacing it.
        if !self.disk_matches(rel, entry)? {
            cleanup(self);
            return Ok(Outcome::LocalChanged);
        }

        // 3. Keep the old content.
        let result = (|| -> Result<()> {
            if entry.is_some_and(|e| !e.record.deleted) {
                if conflict {
                    let loser = entry.map(|e| e.record.clone());
                    if let Some(loser) = loser {
                        self.save_conflict_copy(&loser)?;
                    }
                } else {
                    self.archive_copy(rel)?;
                }
            }
            // 4. Atomic rename into place.
            self.fs.rename(&self.abs(&tmp), &self.abs(rel))?;
            Ok(())
        })();
        if let Err(e) = result {
            cleanup(self);
            return Err(e);
        }

        let meta = self.fs.metadata(&self.abs(rel))?;
        self.index.put(record, Some(stat_of(&meta)))?;
        Ok(Outcome::Done)
    }

    /// Write `record`'s content to `tmp`. Returns `Some(outcome)` if it could
    /// not be completed for a non-error reason.
    fn write_temp(
        &mut self,
        tmp: &str,
        record: &FileRecord,
        source: &mut dyn ChunkSource,
        stats: &mut SyncStats,
    ) -> Result<Option<Outcome>> {
        let mut missing: Option<Hash> = None;
        let mut failure: Option<EngineError> = None;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        let this: &Self = self;
        let mut fill = |w: &mut dyn Write| -> io::Result<()> {
            for c in &record.chunks {
                let data = match this.chunk_data(c, source, stats) {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        missing = Some(c.hash);
                        return Err(io::Error::other("chunk unavailable"));
                    }
                    Err(e) => {
                        failure = Some(e);
                        return Err(io::Error::other("chunk read failed"));
                    }
                };
                hasher.update(&data);
                size += data.len() as u64;
                w.write_all(&data)?;
            }
            Ok(())
        };
        let written = self.fs.create_new_with(&self.abs(tmp), &mut fill);
        if let Some(h) = missing {
            return Ok(Some(Outcome::MissingChunks(vec![h])));
        }
        if let Some(e) = failure {
            return Err(e);
        }
        written?;
        if size != record.size || Hash::from(hasher.finalize()) != record.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("assembled {} does not match its hash", record.path),
            )
            .into());
        }
        Ok(None)
    }

    fn chunk_data(
        &self,
        chunk: &ChunkRef,
        source: &mut dyn ChunkSource,
        stats: &mut SyncStats,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(d) = self.local_chunk(&chunk.hash)? {
            stats.chunks_reused += 1;
            return Ok(Some(d));
        }
        Ok(source
            .get(&chunk.hash)
            .filter(|d| Hash::of(d) == chunk.hash)
            .inspect(|_| stats.chunks_fetched += 1))
    }

    /// Store `records` as what `peer` has, then plan and apply until nothing
    /// more can be done, fetching chunks from `source`. Used for in-process
    /// sync; the networked engine drives the same steps from messages.
    pub fn sync_from(
        &mut self,
        peer: DeviceId,
        records: &[FileRecord],
        source: &mut dyn ChunkSource,
    ) -> Result<SyncStats> {
        self.index.batch(|index| {
            for r in records {
                index.put_remote(&peer, r)?;
            }
            Ok::<_, EngineError>(())
        })?;
        self.converge(source)
    }

    /// Plan and apply repeatedly until no action makes progress.
    pub fn converge(&mut self, source: &mut dyn ChunkSource) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        loop {
            let mut progress = false;
            stats.blocked.clear();
            for action in self.plan_all()? {
                let path = action.record().path.clone();
                match self.apply(&action, source, &mut stats)? {
                    Outcome::Done | Outcome::Stale => progress = true,
                    Outcome::LocalChanged => {
                        if !self.scan(&path)?.changed.is_empty() {
                            progress = true;
                        }
                    }
                    Outcome::MissingChunks(_) => {
                        stats.blocked.push((path, "missing chunks".into()));
                    }
                    Outcome::Blocked(why) => stats.blocked.push((path, why)),
                }
            }
            if !progress {
                return Ok(stats);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bsync_fs::StdFs;

    use super::*;

    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn versions_cleanup_by_age_then_size() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let v = root.join(".bsync/versions/sub");
        std::fs::create_dir_all(&v).unwrap();
        // 2024-01-01 .. 2024-01-04, 100 bytes each.
        for (day, name) in [
            (1, "a~20240101-120000.txt"),
            (2, "b~20240102-120000"),
            (3, "sub-c~20240103-120000.md"),
            (4, "d~20240104-120000.txt"),
        ] {
            let dir = if day == 3 {
                v.clone()
            } else {
                root.join(".bsync/versions")
            };
            std::fs::write(dir.join(name.trim_start_matches("sub-")), vec![0u8; 100]).unwrap();
        }
        // "Now" is 2024-01-05 12:00 UTC.
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_704_456_000);
        let f = Folder::new(
            Arc::new(StdFs),
            Arc::new(FixedClock(now)),
            root.to_path_buf(),
            DeviceId::from_bytes([1; 32]),
            ChunkParams::DEFAULT,
            Index::open_in_memory().unwrap(),
        );
        let day = std::time::Duration::from_secs(86_400);
        // Older than 3.5 days: only a (Jan 1).
        assert_eq!(f.clean_versions(day * 7 / 2, u64::MAX).unwrap(), 1);
        assert!(!root.join(".bsync/versions/a~20240101-120000.txt").exists());
        // Size cap of 200 bytes: the oldest remaining (b) goes.
        assert_eq!(f.clean_versions(day * 30, 200).unwrap(), 1);
        assert!(!root.join(".bsync/versions/b~20240102-120000").exists());
        assert!(v.join("c~20240103-120000.md").exists());
        assert!(root.join(".bsync/versions/d~20240104-120000.txt").exists());
    }

    #[test]
    fn split_helpers() {
        assert_eq!(split_ext("a.txt"), ("a", ".txt"));
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
        assert_eq!(split_ext("noext"), ("noext", ""));
        assert_eq!(split_dir("a/b/c.txt"), ("a/b", "c.txt"));
        assert_eq!(split_dir("c.txt"), ("", "c.txt"));
    }
}
