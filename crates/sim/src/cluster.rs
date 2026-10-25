//! A cluster of [`SyncEngine`]s on in-memory file systems, connected by a
//! [`SimNetwork`], driven step by step from a seeded RNG.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bsync_core::{ChunkParams, DeviceId, FolderId};
use bsync_engine::{Clock, EngineConfig, Folder, SyncEngine};
use bsync_fs::FileSystem;
use bsync_index::Index;
use bsync_proto::Message;

use crate::{MemFs, SimNetwork, SimRng};

/// Deterministic clock advanced by the simulation.
#[derive(Debug, Clone, Default)]
pub struct SimClock(Arc<AtomicU64>);

impl SimClock {
    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_millis() as u64, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_700_000_000)
            + Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
}

pub const ROOT: &str = "/sync";

pub fn device(n: usize) -> DeviceId {
    let mut b = [0u8; 32];
    b[0] = n as u8 + 1;
    b[31] = 0x5A;
    DeviceId::from_bytes(b)
}

pub struct Node {
    pub id: DeviceId,
    pub fs: MemFs,
    /// `None` while the node is "restarting".
    pub engine: Option<SyncEngine>,
    /// Kept while the engine is down, so a restart keeps the index.
    parked: Option<Folder>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Cluster {
    pub rng: SimRng,
    pub net: SimNetwork,
    pub clock: SimClock,
    pub nodes: Vec<Node>,
    pub folder: FolderId,
}

impl Cluster {
    /// `n` devices sharing one folder, all connected.
    pub fn new(seed: u64, n: usize) -> Self {
        let mut c = Self {
            rng: SimRng::new(seed),
            net: SimNetwork::new(),
            clock: SimClock::default(),
            nodes: Vec::new(),
            folder: FolderId("sim".into()),
        };
        for i in 0..n {
            let fs = MemFs::new();
            fs.create_dir(Path::new(ROOT)).expect("fresh fs");
            let id = device(i);
            let folder = Folder::new(
                Arc::new(fs.clone()),
                Arc::new(c.clock.clone()),
                PathBuf::from(ROOT),
                id,
                ChunkParams::TINY,
                Index::open_in_memory().expect("in-memory index"),
            );
            c.nodes.push(Node {
                id,
                fs,
                engine: None,
                parked: Some(folder),
            });
        }
        for i in 0..n {
            c.start(i);
        }
        c
    }

    fn ids(&self) -> Vec<DeviceId> {
        self.nodes.iter().map(|n| n.id).collect()
    }

    /// Start node `i`'s engine and connect it to every reachable peer.
    pub fn start(&mut self, i: usize) {
        let Some(folder) = self.nodes[i].parked.take() else {
            return;
        };
        let me = self.nodes[i].id;
        let others: BTreeSet<DeviceId> = self.ids().into_iter().filter(|d| *d != me).collect();
        let index_id = self.rng.next_u64();
        let mut engine = SyncEngine::new(
            folder,
            EngineConfig::new(self.folder.clone(), others),
            Arc::new(self.net.endpoint(me)),
            Arc::new(self.clock.clone()),
            || index_id,
        )
        .expect("engine starts");
        engine.full_scan().expect("startup scan");
        self.nodes[i].engine = Some(engine);
        for j in 0..self.nodes.len() {
            if j != i
                && self.nodes[j].engine.is_some()
                && self.net.is_connected(me, self.nodes[j].id)
            {
                self.connect_pair(i, j);
            }
        }
    }

    /// Stop node `i`'s engine (its disk and index survive).
    pub fn stop(&mut self, i: usize) {
        let Some(engine) = self.nodes[i].engine.take() else {
            return;
        };
        let me = self.nodes[i].id;
        self.nodes[i].parked = Some(engine.into_folder());
        // Peers notice the connection drop; anything in flight is lost.
        for j in 0..self.nodes.len() {
            if j != i {
                let other = self.nodes[j].id;
                if self.net.is_connected(me, other) {
                    self.net.partition(me, other);
                    self.net.heal(me, other);
                }
                if let Some(e) = self.nodes[j].engine.as_mut() {
                    e.on_disconnected(me);
                }
            }
        }
    }

    fn connect_pair(&mut self, i: usize, j: usize) {
        let (a, b) = (self.nodes[i].id, self.nodes[j].id);
        if let Some(e) = self.nodes[i].engine.as_mut() {
            e.on_connected(b).expect("on_connected");
        }
        if let Some(e) = self.nodes[j].engine.as_mut() {
            e.on_connected(a).expect("on_connected");
        }
    }

    /// Cut the link between nodes `i` and `j`.
    pub fn partition(&mut self, i: usize, j: usize) {
        let (a, b) = (self.nodes[i].id, self.nodes[j].id);
        if !self.net.is_connected(a, b) {
            return;
        }
        self.net.partition(a, b);
        if let Some(e) = self.nodes[i].engine.as_mut() {
            e.on_disconnected(b);
        }
        if let Some(e) = self.nodes[j].engine.as_mut() {
            e.on_disconnected(a);
        }
    }

    pub fn heal(&mut self, i: usize, j: usize) {
        let (a, b) = (self.nodes[i].id, self.nodes[j].id);
        if self.net.is_connected(a, b) {
            return;
        }
        self.net.heal(a, b);
        if self.nodes[i].engine.is_some() && self.nodes[j].engine.is_some() {
            self.connect_pair(i, j);
        }
    }

    pub fn heal_all(&mut self) {
        for i in 0..self.nodes.len() {
            for j in i + 1..self.nodes.len() {
                self.heal(i, j);
            }
        }
    }

    fn index_of(&self, id: DeviceId) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    /// Deliver one message, if any. Returns whether one was delivered.
    pub fn deliver_one(&mut self) -> bool {
        let Some(d) = self.net.deliver_one(&mut self.rng) else {
            return false;
        };
        let Some(to) = self.index_of(d.to) else {
            return true;
        };
        let Some(engine) = self.nodes[to].engine.as_mut() else {
            return true;
        };
        let msg = Message::decode(&d.msg).expect("valid message");
        engine.on_message(d.from, msg).expect("on_message");
        true
    }

    pub fn tick(&mut self, i: usize) {
        if let Some(e) = self.nodes[i].engine.as_mut() {
            e.tick().expect("tick");
        }
    }

    pub fn tick_all(&mut self) {
        for i in 0..self.nodes.len() {
            self.tick(i);
        }
    }

    /// Tell node `i` that `paths` changed on disk (what the watcher does).
    pub fn notify_change(&mut self, i: usize, paths: &[String]) {
        if let Some(e) = self.nodes[i].engine.as_mut() {
            e.on_local_change(paths).expect("scan");
        }
    }

    pub fn full_scan(&mut self, i: usize) {
        if let Some(e) = self.nodes[i].engine.as_mut() {
            e.full_scan().expect("scan");
        }
    }

    /// Run until no messages are in flight and every engine is idle.
    /// Returns false if that did not happen within `max_steps`.
    pub fn settle(&mut self, max_steps: usize) -> bool {
        for _ in 0..max_steps {
            self.tick_all();
            let mut delivered = false;
            for _ in 0..64 {
                if !self.deliver_one() {
                    break;
                }
                delivered = true;
            }
            if delivered {
                continue;
            }
            self.tick_all();
            let idle = self.net.in_flight() == 0
                && self
                    .nodes
                    .iter()
                    .all(|n| n.engine.as_ref().is_none_or(SyncEngine::is_idle));
            if idle {
                return true;
            }
            // Let request timeouts fire.
            self.clock.advance(Duration::from_secs(31));
        }
        false
    }

    /// Node `i`'s folder content (excluding `.bsync`).
    pub fn tree(&self, i: usize) -> std::collections::BTreeMap<String, Option<Arc<Vec<u8>>>> {
        let mut t = self.nodes[i]
            .fs
            .snapshot(Path::new(ROOT))
            .expect("snapshot");
        t.retain(|k, _| !k.starts_with(".bsync"));
        t
    }

    pub fn path(rel: &str) -> PathBuf {
        bsync_fs::rel_to_path(Path::new(ROOT), rel)
    }
}
