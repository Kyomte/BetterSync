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

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bsync_core::plan::{Action, apply_order, conflict_copy_path, plan_path};
use bsync_core::{
    ChunkParams, ChunkRef, DeviceId, EntryKind, FileRecord, Hash, chunk_reader, format_utc_compact,
};
use bsync_fs::{BSYNC_DIR, FileKind, FileSystem, Metadata, TEMP_PREFIX, mtime_ns, rel_to_path};
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
}

impl std::fmt::Debug for Folder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Folder")
            .field("root", &self.root)
            .field("device", &self.device)
            .finish_non_exhaustive()
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
        }
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

    pub fn abs(&self, rel: &str) -> PathBuf {
        rel_to_path(&self.root, rel)
    }

    /// Whether `rel` is excluded from syncing.
    pub fn is_skipped(&self, rel: &str, meta: &Metadata) -> bool {
        (self.skip)(rel, meta)
    }

    /// Scan `subtree` (`""` = everything) for local changes.
    pub fn scan(&mut self, subtree: &str) -> Result<ScanStats> {
        let ctx = ScanContext {
            fs: &*self.fs,
            root: &self.root,
            device: self.device,
            params: self.params,
            skip: &*self.skip,
        };
        crate::scan::scan(&ctx, &mut self.index, subtree)
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
                if record.deleted {
                    self.apply_delete(record, entry.as_ref())?
                } else if record.kind == EntryKind::Dir {
                    self.apply_dir(record, entry.as_ref(), loser.is_some())?
                } else {
                    self.apply_file(record, entry.as_ref(), loser.is_some(), source, stats)?
                }
            }
        };
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
                let mut r = new_version(entry.as_ref().map(|e| &e.record), &dir, self.device);
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
        let mut r = new_version(prev.as_ref().map(|e| &e.record), &copy, self.device);
        r.kind = EntryKind::File;
        r.size = loser.size;
        r.hash = loser.hash;
        r.chunks = loser.chunks.clone();
        r.mtime_ns = mtime_ns(meta.modified);
        self.index.put(&r, Some(stat_of(&meta)))?;
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
                    let mut r = new_version(entry.map(|e| &e.record), rel, self.device);
                    r.kind = EntryKind::Dir;
                    r.version = record
                        .version
                        .merged(&entry.map(|e| e.record.version.clone()).unwrap_or_default())
                        .incremented(self.device);
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
        if let Some(m) = self.disk(rel)? {
            if m.kind == FileKind::Dir {
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
                    Outcome::Done => progress = true,
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
    use super::*;

    #[test]
    fn split_helpers() {
        assert_eq!(split_ext("a.txt"), ("a", ".txt"));
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
        assert_eq!(split_ext("noext"), ("noext", ""));
        assert_eq!(split_dir("a/b/c.txt"), ("a/b", "c.txt"));
        assert_eq!(split_dir("c.txt"), ("", "c.txt"));
    }
}
