//! The networked sync engine for one folder.
//!
//! Message-driven and synchronous: whoever drives it (the daemon or the
//! simulator) feeds in connection events, messages and local changes, and
//! calls [`SyncEngine::tick`]. Outgoing messages go through a
//! [`Transport`]. Nothing here sleeps or spawns threads, which keeps the
//! simulator deterministic.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bsync_core::{DeviceId, FileRecord, FolderId, Hash, is_valid_rel_path};
use bsync_fs::{BSYNC_DIR, FileSystem, rel_to_path};
use bsync_index::PeerProgress;
use bsync_net::Transport;
use bsync_proto::{INDEX_BATCH, Message};

use crate::folder::{ChunkSource, Clock, Folder, Outcome, SyncStats};
use crate::{EngineError, Result};

const CHUNK_DIR: &str = ".bsync/chunks";

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub folder_id: FolderId,
    /// Devices this folder is shared with. Messages from anyone else about
    /// this folder are dropped.
    pub shared_with: BTreeSet<DeviceId>,
    pub max_in_flight: usize,
    pub request_timeout: Duration,
    /// Old versions in `.bsync/versions/` are removed after this long...
    pub versions_max_age: Duration,
    /// ...or, oldest first, while they take more than this many bytes.
    pub versions_max_bytes: u64,
    /// On a metered network, files larger than this are neither fetched
    /// nor served (`None`: no limit).
    pub metered_max_file: Option<u64>,
}

impl EngineConfig {
    pub fn new(folder_id: FolderId, shared_with: impl IntoIterator<Item = DeviceId>) -> Self {
        Self {
            folder_id,
            shared_with: shared_with.into_iter().collect(),
            max_in_flight: 32,
            request_timeout: Duration::from_secs(30),
            versions_max_age: Duration::from_secs(30 * 24 * 3600),
            versions_max_bytes: 10 * 1024 * 1024 * 1024,
            metered_max_file: Some(50 * 1024 * 1024),
        }
    }
}

#[derive(Debug, Default)]
struct PeerState {
    /// The peer asked for our index; this is how far we have sent it.
    sent_seq: Option<u64>,
    /// Their request named an index incarnation that is not ours.
    reset_pending: bool,
}

#[derive(Debug, Clone)]
struct Request {
    peer: DeviceId,
    hash: Hash,
    sent: SystemTime,
}

/// What the engine is up to, for status displays and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineStatus {
    pub connected: Vec<DeviceId>,
    /// Paths that still need work.
    pub pending: usize,
    pub in_flight: usize,
    /// Chunks requested from peers since the engine started.
    pub chunks_requested: u64,
    /// Paths that cannot be synced right now, with the reason.
    pub blocked: Vec<(String, String)>,
    pub totals: SyncStats,
}

/// Fetched chunks waiting to be assembled, persisted in `.bsync/chunks/`
/// so an interrupted transfer resumes where it stopped.
struct ChunkCache {
    fs: Arc<dyn FileSystem>,
    root: PathBuf,
}

impl ChunkCache {
    fn rel(hash: &Hash) -> String {
        format!("{CHUNK_DIR}/{}", hash.to_hex())
    }

    fn path(&self, hash: &Hash) -> PathBuf {
        rel_to_path(&self.root, &Self::rel(hash))
    }
}

impl ChunkSource for ChunkCache {
    fn has(&self, hash: &Hash) -> bool {
        self.fs.metadata(&self.path(hash)).is_ok()
    }

    fn get(&mut self, hash: &Hash) -> Option<Vec<u8>> {
        let data = self.fs.read(&self.path(hash)).ok()?;
        (Hash::of(&data) == *hash).then_some(data)
    }
}

pub struct SyncEngine {
    folder: Folder,
    config: EngineConfig,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    index_id: u64,
    peers: BTreeMap<DeviceId, PeerState>,
    /// Paths whose plan must be (re)evaluated.
    pending: BTreeSet<String>,
    /// Chunks needed, with the path that needs them, not yet requested.
    wanted: VecDeque<(Hash, String)>,
    in_flight: BTreeMap<u64, Request>,
    /// Peers that told us they lack a chunk.
    /// Peers that recently could not provide a chunk, until when to avoid
    /// asking them again.
    lacking: BTreeMap<(DeviceId, Hash), (SystemTime, u32)>,
    /// Work is stalled (chunks nobody could provide, paths that failed);
    /// try again then.
    retry_at: Option<SystemTime>,
    /// Paths whose last attempt failed with an I/O error (e.g. the file is
    /// locked by another program): failures so far, and when to retry.
    failing: BTreeMap<String, (u32, SystemTime)>,
    chunks_requested: u64,
    /// The network is metered (mobile data, hotspot): hold back large files.
    metered: bool,
    next_request: u64,
    /// Something happened that may let pending work proceed.
    dirty: bool,
    paused: bool,
    blocked: Vec<(String, String)>,
    totals: SyncStats,
    temp_counter: u64,
}

impl std::fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("folder", &self.config.folder_id)
            .field("device", &self.folder.device())
            .finish_non_exhaustive()
    }
}

impl SyncEngine {
    /// `new_index_id` supplies a random ID the first time this folder's
    /// index is used.
    pub fn new(
        folder: Folder,
        config: EngineConfig,
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        new_index_id: impl FnOnce() -> u64,
    ) -> Result<Self> {
        let index_id = folder.index().index_id_or_init(new_index_id)?;
        // Anything peers announced earlier may still need applying.
        let pending = folder.index().remote_paths()?.into_iter().collect();
        Ok(Self {
            folder,
            config,
            transport,
            clock,
            index_id,
            peers: BTreeMap::new(),
            pending,
            wanted: VecDeque::new(),
            in_flight: BTreeMap::new(),
            lacking: BTreeMap::new(),
            retry_at: None,
            failing: BTreeMap::new(),
            chunks_requested: 0,
            metered: false,
            next_request: 1,
            dirty: true,
            paused: false,
            blocked: Vec::new(),
            totals: SyncStats::default(),
            temp_counter: 0,
        })
    }

    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    /// Stop the engine, keeping its folder (and index) — a restart.
    pub fn into_folder(self) -> Folder {
        self.folder
    }

    pub fn folder_mut(&mut self) -> &mut Folder {
        &mut self.folder
    }

    pub fn folder_id(&self) -> &FolderId {
        &self.config.folder_id
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Share with a new device / stop sharing (pairing changes).
    pub fn set_shared_with(&mut self, devices: BTreeSet<DeviceId>) {
        self.peers.retain(|p, _| devices.contains(p));
        self.config.shared_with = devices;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.dirty = true;
    }

    /// Tell the engine whether the current network is metered. Large files
    /// wait for an unmetered network.
    pub fn set_metered(&mut self, metered: bool) {
        if self.metered != metered {
            self.metered = metered;
            // Held-back files may go now (or must stop being requested).
            self.pending
                .extend(self.folder.index().remote_paths().unwrap_or_default());
            self.wanted.clear();
            self.dirty = true;
        }
    }

    /// Change the metered-network file size limit (`None`: no limit).
    pub fn set_metered_max_file(&mut self, max: Option<u64>) {
        self.config.metered_max_file = max;
        self.dirty = true;
    }

    pub fn is_metered(&self) -> bool {
        self.metered
    }

    /// Whether a file of `size` bytes must wait for an unmetered network.
    fn held_back(&self, size: u64) -> bool {
        self.metered && self.config.metered_max_file.is_some_and(|max| size > max)
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            connected: self.peers.keys().copied().collect(),
            pending: self.pending.len(),
            in_flight: self.in_flight.len(),
            chunks_requested: self.chunks_requested,
            blocked: self.blocked.clone(),
            totals: self.totals.clone(),
        }
    }

    /// No work left that could make progress on its own.
    pub fn is_idle(&self) -> bool {
        !self.dirty
            && self.in_flight.is_empty()
            && self.wanted.is_empty()
            && self.retry_at.is_none()
    }

    /// When [`SyncEngine::tick`] must run again even if nothing else happens.
    pub fn next_deadline(&self) -> Option<SystemTime> {
        self.in_flight
            .values()
            .map(|r| r.sent + self.config.request_timeout)
            .chain(self.retry_at)
            .min()
    }

    /// `peer` said it lacks `hash` (or did not answer). Ask it again only
    /// after a pause that doubles each time (30 s .. 1 h with the default
    /// timeout), so a peer that refuses — e.g. on a metered network — is not
    /// asked over and over.
    fn mark_lacking(&mut self, peer: DeviceId, hash: Hash) {
        let strikes = self.lacking.get(&(peer, hash)).map_or(0, |l| l.1);
        let wait = self
            .config
            .request_timeout
            .saturating_mul(1 << strikes.min(7))
            .min(Duration::from_secs(3600));
        self.lacking
            .insert((peer, hash), (self.clock.now() + wait, strikes + 1));
    }

    fn lacks(&self, peer: &DeviceId, hash: &Hash) -> bool {
        let now = self.clock.now();
        self.lacking
            .get(&(*peer, *hash))
            .is_some_and(|(until, _)| *until > now)
    }

    fn send(&self, to: DeviceId, msg: &Message) {
        if let Err(e) = self.transport.send(to, msg.encode()) {
            tracing::debug!("send to {to:?} failed: {e}");
        }
    }

    // ----- events -----

    pub fn on_connected(&mut self, peer: DeviceId) -> Result<()> {
        if !self.config.shared_with.contains(&peer) {
            return Ok(());
        }
        self.peers.insert(peer, PeerState::default());
        let progress = self.folder.index().peer_progress(&peer)?;
        self.send(
            peer,
            &Message::IndexRequest {
                folder: self.config.folder_id.clone(),
                known_index_id: progress.map_or(0, |p| p.index_id),
                since_seq: progress.map_or(0, |p| p.max_seq),
            },
        );
        Ok(())
    }

    pub fn on_disconnected(&mut self, peer: DeviceId) {
        if self.peers.remove(&peer).is_none() {
            return;
        }
        let dropped: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, r)| r.peer == peer)
            .map(|(id, _)| *id)
            .collect();
        for id in dropped {
            self.in_flight.remove(&id);
        }
        self.lacking.retain(|(p, _), _| *p != peer);
        self.dirty = true;
    }

    /// Paths changed on disk (from the watcher).
    pub fn on_local_change(&mut self, paths: &[String]) -> Result<()> {
        for p in paths {
            self.scan(p)?;
        }
        Ok(())
    }

    /// Rescan everything (at startup and periodically) and prune old
    /// versions.
    pub fn full_scan(&mut self) -> Result<()> {
        self.scan("")?;
        let removed = self
            .folder
            .clean_versions(self.config.versions_max_age, self.config.versions_max_bytes)?;
        if removed > 0 {
            tracing::info!(folder = %self.config.folder_id, removed, "pruned old versions");
        }
        self.folder.prune_bases()?;
        Ok(())
    }

    /// A local change can turn a version we were waiting on into one we
    /// must act on (e.g. deleting our side of a conflict makes us the
    /// loser), so changed paths are planned again.
    fn scan(&mut self, subtree: &str) -> Result<()> {
        let stats = self.folder.scan(subtree)?;
        self.pending.extend(stats.changed);
        self.dirty = true;
        Ok(())
    }

    pub fn on_message(&mut self, peer: DeviceId, msg: Message) -> Result<()> {
        if !self.config.shared_with.contains(&peer) || !self.peers.contains_key(&peer) {
            tracing::warn!("dropping message from {peer:?}: not shared or not connected");
            return Ok(());
        }
        if msg.folder() != Some(&self.config.folder_id) {
            return Ok(());
        }
        match msg {
            Message::IndexRequest {
                known_index_id,
                since_seq,
                ..
            } => {
                let state = self.peers.entry(peer).or_default();
                if known_index_id == self.index_id {
                    state.sent_seq = Some(since_seq);
                    state.reset_pending = false;
                } else {
                    state.sent_seq = Some(0);
                    state.reset_pending = true;
                }
                self.push_index()?;
            }
            Message::IndexUpdate {
                index_id,
                reset,
                records,
                last_seq,
                ..
            } => self.receive_index(peer, index_id, reset, records, last_seq)?,
            Message::RequestChunk { id, hash, .. } => {
                // Uploading large files costs metered data too.
                let locations = self.folder.index().find_chunk(&hash)?;
                let large = self.metered
                    && !locations.is_empty()
                    && locations.iter().all(|loc| {
                        self.folder
                            .index()
                            .get(&loc.path)
                            .ok()
                            .flatten()
                            .is_some_and(|e| self.held_back(e.record.size))
                    });
                let data = if large { None } else { self.read_chunk(&hash)? };
                self.send(
                    peer,
                    &Message::ChunkData {
                        folder: self.config.folder_id.clone(),
                        id,
                        hash,
                        data,
                    },
                );
            }
            Message::ChunkData { id, hash, data, .. } => {
                let Some(req) = self.in_flight.remove(&id) else {
                    return Ok(());
                };
                match data {
                    Some(d) if Hash::of(&d) == hash && req.hash == hash => {
                        self.store_chunk(&hash, &d)?;
                        self.dirty = true;
                    }
                    _ => {
                        self.mark_lacking(peer, hash);
                        self.dirty = true;
                    }
                }
                self.request_chunks();
            }
            Message::Hello { .. } | Message::Ping | Message::Pong => {}
        }
        Ok(())
    }

    fn receive_index(
        &mut self,
        peer: DeviceId,
        index_id: u64,
        reset: bool,
        records: Vec<FileRecord>,
        last_seq: u64,
    ) -> Result<()> {
        let mut paths = Vec::with_capacity(records.len());
        self.folder.index_mut().batch(|index| {
            if reset {
                index.clear_remote(&peer)?;
            }
            for r in &records {
                if !is_valid_rel_path(&r.path) {
                    tracing::warn!(path = %r.path, "peer {peer:?} sent an invalid path; ignored");
                    continue;
                }
                index.put_remote(&peer, r)?;
                paths.push(r.path.clone());
            }
            index.set_peer_progress(
                &peer,
                PeerProgress {
                    index_id,
                    max_seq: last_seq,
                },
            )?;
            Ok::<_, EngineError>(())
        })?;
        if reset {
            // Their old records are gone; anything may need re-planning.
            self.pending.extend(self.folder.index().remote_paths()?);
        }
        self.pending.extend(paths);
        // What they lacked before they may have now.
        self.lacking.retain(|(p, _), _| *p != peer);
        self.dirty = true;
        Ok(())
    }

    // ----- chunks -----

    fn cache(&self) -> ChunkCache {
        ChunkCache {
            fs: self.folder.fs_arc(),
            root: self.folder.root().to_path_buf(),
        }
    }

    fn read_chunk(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        if let Some(d) = self.folder.local_chunk(hash)? {
            return Ok(Some(d));
        }
        Ok(self.cache().get(hash))
    }

    fn store_chunk(&mut self, hash: &Hash, data: &[u8]) -> Result<()> {
        let fs = self.folder.fs();
        let dir = self.folder.abs(CHUNK_DIR);
        for d in [self.folder.abs(BSYNC_DIR), dir] {
            match fs.create_dir(&d) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        let final_path = self.cache().path(hash);
        if fs.metadata(&final_path).is_ok() {
            return Ok(());
        }
        self.temp_counter += 1;
        let tmp = self.folder.abs(&format!(
            "{}.part{}",
            ChunkCache::rel(hash),
            self.temp_counter
        ));
        let _ = fs.remove_file(&tmp);
        fs.create_new(&tmp, data)?;
        fs.rename(&tmp, &final_path)?;
        Ok(())
    }

    /// Delete cached chunks once nothing is waiting for them.
    fn clear_chunk_cache(&mut self) -> Result<()> {
        let fs = self.folder.fs();
        let dir = self.folder.abs(CHUNK_DIR);
        let entries = match fs.read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for e in entries {
            let _ = fs.remove_file(&dir.join(&e.name));
        }
        Ok(())
    }

    /// Which connected peer to ask for `hash`, needed by `path`. `holders`
    /// caches, per path, which peer announced which chunks (decoding the
    /// records once per call of [`Self::request_chunks`], not per chunk).
    fn pick_peer(
        &self,
        hash: &Hash,
        path: &str,
        holders: &mut BTreeMap<String, Vec<(DeviceId, BTreeSet<Hash>)>>,
    ) -> Result<Option<DeviceId>> {
        let load = |p: &DeviceId| self.in_flight.values().filter(|r| r.peer == *p).count();
        let usable = |p: &DeviceId| self.peers.contains_key(p) && !self.lacks(p, hash);
        if !holders.contains_key(path) {
            let list = self
                .folder
                .index()
                .remote(path)?
                .into_iter()
                .map(|(p, r)| (p, r.chunks.iter().map(|c| c.hash).collect()))
                .collect();
            holders.insert(path.to_owned(), list);
        }
        let mut candidates: Vec<DeviceId> = holders[path]
            .iter()
            .filter(|(p, chunks)| usable(p) && chunks.contains(hash))
            .map(|(p, _)| *p)
            .collect();
        if candidates.is_empty() {
            candidates = self.peers.keys().copied().filter(|p| usable(p)).collect();
        }
        Ok(candidates.into_iter().min_by_key(|p| (load(p), *p)))
    }

    fn request_chunks(&mut self) {
        let mut unavailable = VecDeque::new();
        let mut holders = BTreeMap::new();
        while self.in_flight.len() < self.config.max_in_flight {
            let Some((hash, path)) = self.wanted.pop_front() else {
                break;
            };
            // Already on its way, or arrived since it was queued.
            if self.in_flight.values().any(|r| r.hash == hash) || self.cache().has(&hash) {
                continue;
            }
            let peer = match self.pick_peer(&hash, &path, &mut holders) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    // Nobody can provide it right now; keep wanting it.
                    unavailable.push_back((hash, path));
                    continue;
                }
                Err(e) => {
                    tracing::warn!("choosing a peer for a chunk failed: {e}");
                    unavailable.push_back((hash, path));
                    continue;
                }
            };
            let id = self.next_request;
            self.next_request += 1;
            self.chunks_requested += 1;
            self.in_flight.insert(
                id,
                Request {
                    peer,
                    hash,
                    sent: self.clock.now(),
                },
            );
            self.send(
                peer,
                &Message::RequestChunk {
                    folder: self.config.folder_id.clone(),
                    id,
                    hash,
                },
            );
        }
        self.wanted.extend(unavailable);
        self.update_retry();
    }

    /// When to try again without any outside event: chunks no connected
    /// peer could provide, and paths backing off after errors.
    fn update_retry(&mut self) {
        let now = self.clock.now();
        // When the first peer that refused a wanted chunk may be asked again.
        let chunks = (!self.wanted.is_empty() && self.in_flight.is_empty()).then(|| {
            self.lacking
                .values()
                .map(|(until, _)| *until)
                .filter(|until| *until > now)
                .min()
                .unwrap_or(now + self.config.request_timeout)
        });
        let failures = self.failing.values().map(|(_, at)| *at).min();
        self.retry_at = chunks.into_iter().chain(failures).min();
    }

    // ----- the main step -----

    /// Do whatever work is possible now: expire timed-out requests, apply
    /// remote changes, request missing chunks, and send index updates.
    pub fn tick(&mut self) -> Result<()> {
        let now = self.clock.now();
        let timeout = self.config.request_timeout;
        let expired: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, r)| r.sent + timeout <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(r) = self.in_flight.remove(&id) {
                self.mark_lacking(r.peer, r.hash);
                self.dirty = true;
            }
        }
        if self.retry_at.is_some_and(|t| t <= now) {
            self.retry_at = None;
            self.dirty = true;
        }
        if self.dirty && !self.paused {
            self.dirty = false;
            self.pull()?;
        }
        self.push_index()
    }

    fn pull(&mut self) -> Result<()> {
        // The rules may have arrived by sync, or changed on disk.
        self.folder.refresh_ignore()?;
        if self.folder.take_ignore_changed() {
            self.pending.extend(self.folder.index().remote_paths()?);
            self.scan("")?;
            self.folder.take_ignore_changed();
        }
        let mut wanted: Vec<(Hash, String)> = Vec::new();
        loop {
            let mut actions = Vec::new();
            for path in &self.pending {
                if let Some(a) = self.folder.plan(path)? {
                    actions.push(a);
                }
            }
            actions.sort_by(bsync_core::plan::apply_order);
            let mut still_pending = BTreeSet::new();
            let mut progress = false;
            wanted.clear();
            self.blocked.clear();
            let now = self.clock.now();
            for action in actions {
                let path = action.record().path.clone();
                if let Some((n, at)) = self.failing.get(&path)
                    && *at > now
                {
                    self.blocked
                        .push((path.clone(), format!("failed {n} time(s); retrying")));
                    still_pending.insert(path);
                    continue;
                }
                let record = action.record();
                if record.is_live_file() && self.held_back(record.size) {
                    self.blocked.push((
                        path.clone(),
                        "large file: waiting for an unmetered network".into(),
                    ));
                    still_pending.insert(path);
                    continue;
                }
                let mut cache = self.cache();
                let outcome = match self.folder.apply(&action, &mut cache, &mut self.totals) {
                    Ok(o) => {
                        self.failing.remove(&path);
                        o
                    }
                    // One path's trouble (a locked file, a full disk, a
                    // permission problem) must not stop the others.
                    Err(EngineError::Io(e)) => {
                        let n = self.failing.get(&path).map_or(0, |f| f.0) + 1;
                        let wait =
                            Duration::from_secs(1u64 << n.min(8)).min(Duration::from_secs(300));
                        tracing::warn!(%path, "could not apply (attempt {n}), retrying in {wait:?}: {e}");
                        self.failing.insert(path.clone(), (n, now + wait));
                        Outcome::Blocked(format!("{e}"))
                    }
                    Err(e) => return Err(e),
                };
                match outcome {
                    Outcome::Done => progress = true,
                    Outcome::Stale => {
                        progress = true;
                        still_pending.insert(path);
                    }
                    Outcome::LocalChanged => {
                        let changed = self.folder.scan(&path)?.changed;
                        if !changed.is_empty() {
                            progress = true;
                        }
                        still_pending.extend(changed);
                        still_pending.insert(path);
                    }
                    Outcome::MissingChunks(hashes) => {
                        wanted.extend(hashes.into_iter().map(|h| (h, path.clone())));
                        still_pending.insert(path);
                    }
                    Outcome::Blocked(why) => {
                        self.blocked.push((path.clone(), why));
                        still_pending.insert(path);
                    }
                }
            }
            self.pending = still_pending;
            if !progress {
                break;
            }
        }
        // Keep previously wanted chunks that are still wanted.
        let known: BTreeSet<Hash> = wanted.iter().map(|(h, _)| *h).collect();
        self.wanted.retain(|(h, _)| known.contains(h));
        for (h, p) in wanted {
            let requested = self.in_flight.values().any(|r| r.hash == h);
            if !requested && !self.wanted.iter().any(|(w, _)| *w == h) {
                self.wanted.push_back((h, p));
            }
        }
        if self.pending.is_empty() {
            self.lacking.clear();
            self.clear_chunk_cache()?;
        }
        let pending = &self.pending;
        self.failing.retain(|p, _| pending.contains(p));
        self.request_chunks();
        Ok(())
    }

    /// Send index updates to every peer that asked for them.
    fn push_index(&mut self) -> Result<()> {
        let peers: Vec<DeviceId> = self.peers.keys().copied().collect();
        for peer in peers {
            while let Some(state) = self.peers.get(&peer) {
                let Some(sent) = state.sent_seq else { break };
                let reset = state.reset_pending;
                let batch = self.folder.index().since(sent, INDEX_BATCH)?;
                if batch.is_empty() && !reset {
                    break;
                }
                let last_seq = batch.last().map_or(sent, |e| e.seq);
                self.send(
                    peer,
                    &Message::IndexUpdate {
                        folder: self.config.folder_id.clone(),
                        index_id: self.index_id,
                        reset,
                        records: batch.into_iter().map(|e| e.record).collect(),
                        last_seq,
                    },
                );
                if let Some(state) = self.peers.get_mut(&peer) {
                    state.sent_seq = Some(last_seq);
                    state.reset_pending = false;
                }
            }
        }
        Ok(())
    }
}
