//! The daemon: one QUIC endpoint, one engine per folder, file watchers, and
//! a connection loop for paired devices.
//!
//! All engines run on a single "engine thread" that sleeps until an event
//! arrives (network, file change, control request) or an engine deadline
//! passes — there is no polling loop.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bsync_core::{ChunkParams, DeviceId};
use bsync_engine::{EngineConfig, EngineStatus, Folder, SyncEngine, SystemClock};
use bsync_fs::{FolderWatcher, StdFs, WatchConfig, WatchEvent};
use bsync_index::Index;
use bsync_net::{Dialer, Discovery, Identity, NetEvent, PairedSet, QuicNet};

use crate::config::{Config, FolderConfig, MeteredMode};

/// A request to the engine thread, answered on `reply`.
pub enum Control {
    Status(mpsc::Sender<DaemonStatus>),
    /// Re-read config.json (after the CLI changed it).
    Reload(mpsc::Sender<Result<(), String>>),
    Pause(String, bool, mpsc::Sender<Result<(), String>>),
    Rescan(String, mpsc::Sender<Result<(), String>>),
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Control")
    }
}

#[derive(Debug)]
pub enum Event {
    Net(NetEvent),
    Watch(String, WatchEvent),
    /// The OS says the network became metered / unmetered.
    Metered(bool),
    Control(Control),
    Shutdown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    pub id: String,
    pub path: PathBuf,
    pub paused: bool,
    pub error: Option<String>,
    pub engine: EngineStatus,
    /// Local entries (files + dirs, excluding deleted).
    pub local_entries: u64,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub id: DeviceId,
    pub name: String,
    pub connected: bool,
    pub address: Option<SocketAddr>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonStatus {
    /// Large files wait for an unmetered network.
    pub metered: bool,
    pub device_id: Option<DeviceId>,
    pub device_name: String,
    pub listen: Option<SocketAddr>,
    pub folders: Vec<FolderStatus>,
    pub peers: Vec<PeerStatus>,
}

impl DaemonStatus {
    /// Every folder is caught up with what peers announced.
    pub fn is_idle(&self) -> bool {
        self.folders
            .iter()
            .all(|f| f.engine.pending == 0 && f.engine.in_flight == 0)
    }
}

/// Random enough for index incarnation IDs (not for cryptography).
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish()
}

struct FolderRuntime {
    config: FolderConfig,
    engine: Option<SyncEngine>,
    error: Option<String>,
    _watcher: Option<FolderWatcher>,
}

struct EngineThread {
    config_dir: PathBuf,
    config: Config,
    me: DeviceId,
    net: QuicNet,
    folders: Vec<FolderRuntime>,
    events_tx: mpsc::Sender<Event>,
    peers_seen: std::collections::BTreeMap<DeviceId, (String, SocketAddr)>,
    dialer: Arc<Dialer>,
    /// What the OS last said (Auto mode).
    os_metered: bool,
}

/// A running daemon.
pub struct Daemon {
    runtime: Option<tokio::runtime::Runtime>,
    net: QuicNet,
    events: mpsc::Sender<Event>,
    thread: Option<JoinHandle<()>>,
    device_id: DeviceId,
    local_addr: SocketAddr,
    config_dir: PathBuf,
    api_addr: SocketAddr,
    _discovery: Option<Discovery>,
    _metered: Option<bsync_net::metered::MeteredWatcher>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("device_id", &self.device_id)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

/// What the folder's chunking uses (fixed for now).
const CHUNKS: ChunkParams = ChunkParams::DEFAULT;

impl Daemon {
    pub fn start(config_dir: &Path) -> Result<Self> {
        Self::start_with(config_dir, WatchConfig::default())
    }

    pub fn start_with(config_dir: &Path, watch: WatchConfig) -> Result<Self> {
        let config = Config::load_or_default(config_dir)?;
        if !Config::path(config_dir).exists() {
            config.save(config_dir)?;
        }
        let identity = Identity::load_or_create(&Config::identity_path(config_dir))
            .context("loading device identity")?;
        let me = identity.device_id();
        let paired: PairedSet = Arc::new(RwLock::new(config.paired()));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("bsyncd-net")
            .build()?;

        let (events_tx, events_rx) = mpsc::channel::<Event>();
        let sink_tx = Mutex::new(events_tx.clone());
        let folders_cfg = Arc::new(RwLock::new(config.folders.clone()));
        let folders_for = {
            let folders_cfg = Arc::clone(&folders_cfg);
            Arc::new(move |peer: DeviceId| {
                folders_cfg
                    .read()
                    .map(|fs| {
                        fs.iter()
                            .filter(|f| f.shared_with().contains(&peer))
                            .map(FolderConfig::folder_id)
                            .collect()
                    })
                    .unwrap_or_default()
            })
        };
        let listen: SocketAddr = config
            .listen
            .parse()
            .with_context(|| format!("bad listen address {:?}", config.listen))?;
        let net = {
            let _guard = runtime.enter();
            QuicNet::bind(
                listen,
                identity,
                Arc::clone(&paired),
                config.device_name.clone(),
                folders_for,
                Arc::new(move |e| {
                    if let Ok(tx) = sink_tx.lock() {
                        let _ = tx.send(Event::Net(e));
                    }
                }),
            )?
        };
        let local_addr = net.local_addr()?;
        tracing::info!(device = %me, %local_addr, "daemon started");

        // Keep paired devices connected, using static and mDNS addresses.
        let dialer = {
            let _guard = runtime.enter();
            Dialer::start(net.clone())
        };
        dialer.set_devices(config.dial_targets());
        let discovery = if config.local_discovery {
            let d = Arc::clone(&dialer);
            Discovery::start(me, local_addr.port(), move |peer, addrs| {
                d.discovered(peer, addrs)
            })
            .map_err(|e| tracing::warn!("LAN discovery unavailable: {e}"))
            .ok()
        } else {
            None
        };

        // Follow the OS's metered-network state (Windows, macOS).
        let metered_watcher = {
            let tx = Mutex::new(events_tx.clone());
            bsync_net::metered::watch(move |m| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(Event::Metered(m));
                }
            })
        };

        let api_listen: SocketAddr = config
            .api_listen
            .parse()
            .with_context(|| format!("bad api_listen address {:?}", config.api_listen))?;
        let api_addr =
            runtime.block_on(crate::api::serve(api_listen, events_tx.clone(), config_dir))?;
        tracing::info!(%api_addr, "control API listening");

        let mut thread_state = EngineThread {
            config_dir: config_dir.to_path_buf(),
            config: config.clone(),
            me,
            net: net.clone(),
            folders: Vec::new(),
            events_tx: events_tx.clone(),
            peers_seen: Default::default(),
            dialer: Arc::clone(&dialer),
            os_metered: false,
        };
        thread_state.open_folders(watch);
        let thread = std::thread::Builder::new()
            .name("bsyncd-engine".into())
            .spawn(move || thread_state.run(events_rx, paired, folders_cfg, watch))?;

        Ok(Self {
            runtime: Some(runtime),
            net,
            events: events_tx,
            thread: Some(thread),
            device_id: me,
            local_addr,
            config_dir: config_dir.to_path_buf(),
            api_addr,
            _discovery: discovery,
            _metered: metered_watcher,
        })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Where the local control API listens.
    pub fn api_addr(&self) -> SocketAddr {
        self.api_addr
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn net(&self) -> &QuicNet {
        &self.net
    }

    fn ask<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> Control) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.events
            .send(Event::Control(make(tx)))
            .context("daemon stopped")?;
        rx.recv_timeout(Duration::from_secs(30))
            .context("daemon did not answer")
    }

    pub fn status(&self) -> Result<DaemonStatus> {
        self.ask(Control::Status)
    }

    pub fn reload(&self) -> Result<()> {
        self.ask(Control::Reload)?.map_err(anyhow::Error::msg)
    }

    pub fn set_paused(&self, folder: &str, paused: bool) -> Result<()> {
        self.ask(|tx| Control::Pause(folder.to_owned(), paused, tx))?
            .map_err(anyhow::Error::msg)
    }

    pub fn rescan(&self, folder: &str) -> Result<()> {
        self.ask(|tx| Control::Rescan(folder.to_owned(), tx))?
            .map_err(anyhow::Error::msg)
    }

    /// A sender for control requests (used by the HTTP API).
    pub fn events(&self) -> mpsc::Sender<Event> {
        self.events.clone()
    }

    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime lives until drop")
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.events.send(Event::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(rt) = self.runtime.as_ref() {
            rt.block_on(self.net.shutdown());
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EngineThread {
    fn open_folders(&mut self, watch: WatchConfig) {
        let configs = self.config.folders.clone();
        self.folders = configs
            .into_iter()
            .map(|cfg| self.open_folder(cfg, watch))
            .collect();
    }

    fn open_folder(&self, cfg: FolderConfig, watch: WatchConfig) -> FolderRuntime {
        match self.try_open_folder(&cfg, watch) {
            Ok((engine, watcher)) => FolderRuntime {
                config: cfg,
                engine: Some(engine),
                error: None,
                _watcher: watcher,
            },
            Err(e) => {
                tracing::error!(folder = %cfg.id, "cannot start folder: {e:#}");
                FolderRuntime {
                    config: cfg,
                    engine: None,
                    error: Some(format!("{e:#}")),
                    _watcher: None,
                }
            }
        }
    }

    fn try_open_folder(
        &self,
        cfg: &FolderConfig,
        watch: WatchConfig,
    ) -> Result<(SyncEngine, Option<FolderWatcher>)> {
        let index_path = Config::index_path(&self.config_dir, &cfg.id);
        std::fs::create_dir_all(index_path.parent().unwrap_or(Path::new(".")))?;
        let index = Index::open(&index_path)?;
        let marker = cfg.path.join(bsync_fs::BSYNC_DIR);
        if index.max_seq()? == 0 && index.remote_paths()?.is_empty() {
            std::fs::create_dir_all(&marker)
                .with_context(|| format!("creating {}", marker.display()))?;
        } else if !marker.is_dir() {
            // An empty or unmounted folder would look like "everything was
            // deleted". Refuse instead of propagating that.
            anyhow::bail!(
                "{} is missing; is the folder's drive mounted? Not syncing to avoid deleting \
                 everything on other devices.",
                marker.display()
            );
        }
        let folder = Folder::new(
            Arc::new(StdFs),
            Arc::new(SystemClock),
            cfg.path.clone(),
            self.me,
            CHUNKS,
            index,
        );
        let mut engine = SyncEngine::new(
            folder,
            EngineConfig::new(cfg.folder_id(), cfg.shared_with()),
            self.net.transport(),
            Arc::new(SystemClock),
            random_u64,
        )?;
        engine.set_paused(cfg.paused);
        engine.set_metered_max_file(self.config.bandwidth.large_file_bytes());
        engine.set_metered(self.metered());
        engine.full_scan()?;
        let tx = Mutex::new(self.events_tx.clone());
        let id = cfg.id.clone();
        let watcher = FolderWatcher::new(&cfg.path, watch, move |e| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Event::Watch(id.clone(), e));
            }
        })
        .map_err(|e| tracing::warn!(folder = %cfg.id, "file watching unavailable: {e}"))
        .ok();
        Ok((engine, watcher))
    }

    fn run(
        mut self,
        events: mpsc::Receiver<Event>,
        paired: PairedSet,
        folders_cfg: Arc<RwLock<Vec<FolderConfig>>>,
        watch: WatchConfig,
    ) {
        // Peers that connected before the engines existed.
        for peer in self.net.connected_peers() {
            self.on_net(NetEvent::Connected {
                peer,
                name: String::new(),
                addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            });
        }
        self.apply_bandwidth();
        // Work found by the startup scans (no event may come for a while).
        self.tick_all();
        loop {
            let deadline = self
                .folders
                .iter()
                .filter_map(|f| f.engine.as_ref()?.next_deadline())
                .min();
            let event = match deadline {
                None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => {
                    let wait = d
                        .duration_since(SystemTime::now())
                        .unwrap_or(Duration::ZERO);
                    events.recv_timeout(wait)
                }
            };
            let mut batch = Vec::new();
            match event {
                Ok(e) => batch.push(e),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Handle everything already queued before ticking.
            while let Ok(e) = events.try_recv() {
                batch.push(e);
                if batch.len() > 1000 {
                    break;
                }
            }
            for e in batch {
                match e {
                    Event::Shutdown => return,
                    Event::Net(n) => self.on_net(n),
                    Event::Watch(id, w) => self.on_watch(&id, w),
                    Event::Metered(m) => {
                        self.os_metered = m;
                        self.apply_bandwidth();
                    }
                    Event::Control(c) => self.on_control(c, &paired, &folders_cfg, watch),
                }
            }
            self.tick_all();
        }
    }

    /// The effective metered state: configuration first, then the OS.
    fn metered(&self) -> bool {
        match self.config.bandwidth.metered {
            MeteredMode::Yes => true,
            MeteredMode::No => false,
            MeteredMode::Auto => self.os_metered,
        }
    }

    /// Push bandwidth settings to the transport and the engines.
    fn apply_bandwidth(&mut self) {
        let b = &self.config.bandwidth;
        self.net
            .set_rate_limits(b.max_upload_kbps * 1000, b.max_download_kbps * 1000);
        let metered = self.metered();
        let max = b.large_file_bytes();
        tracing::info!(metered, "network policy");
        for f in &mut self.folders {
            if let Some(e) = f.engine.as_mut() {
                e.set_metered_max_file(max);
                e.set_metered(metered);
            }
        }
    }

    fn tick_all(&mut self) {
        for f in &mut self.folders {
            if let Some(e) = f.engine.as_mut()
                && let Err(err) = e.tick()
            {
                tracing::error!(folder = %f.config.id, "sync error: {err}");
                f.error = Some(err.to_string());
            }
        }
    }

    fn on_net(&mut self, event: NetEvent) {
        match event {
            NetEvent::Connected { peer, name, addr } => {
                tracing::info!(%peer, %name, %addr, "connected");
                if !name.is_empty() {
                    self.peers_seen.insert(peer, (name, addr));
                }
                for f in &mut self.folders {
                    if let Some(e) = f.engine.as_mut()
                        && let Err(err) = e.on_connected(peer)
                    {
                        tracing::error!("on_connected: {err}");
                    }
                }
            }
            NetEvent::Disconnected { peer } => {
                tracing::info!(%peer, "disconnected");
                for f in &mut self.folders {
                    if let Some(e) = f.engine.as_mut() {
                        e.on_disconnected(peer);
                    }
                }
            }
            NetEvent::Message { peer, msg } => {
                let Some(folder) = msg.folder().cloned() else {
                    return;
                };
                for f in &mut self.folders {
                    if f.config.id != folder.0 {
                        continue;
                    }
                    if let Some(e) = f.engine.as_mut()
                        && let Err(err) = e.on_message(peer, msg)
                    {
                        tracing::error!(folder = %f.config.id, "message error: {err}");
                    }
                    return;
                }
            }
        }
    }

    fn on_watch(&mut self, id: &str, event: WatchEvent) {
        let Some(f) = self.folders.iter_mut().find(|f| f.config.id == id) else {
            return;
        };
        let Some(e) = f.engine.as_mut() else { return };
        let r = match event {
            WatchEvent::Paths(paths) => e.on_local_change(&paths),
            WatchEvent::FullRescan => e.full_scan(),
        };
        if let Err(err) = r {
            tracing::error!(folder = %f.config.id, "scan error: {err}");
            f.error = Some(err.to_string());
        }
    }

    fn status(&self) -> DaemonStatus {
        let folders = self
            .folders
            .iter()
            .map(|f| {
                let (local_entries, conflicts) = f
                    .engine
                    .as_ref()
                    .and_then(|e| e.folder().index().all().ok())
                    .map(|all| {
                        let live: Vec<_> = all.iter().filter(|e| !e.record.deleted).collect();
                        let conflicts = live
                            .iter()
                            .filter(|e| e.record.path.contains(".sync-conflict-"))
                            .map(|e| e.record.path.clone())
                            .collect();
                        (live.len() as u64, conflicts)
                    })
                    .unwrap_or_default();
                FolderStatus {
                    id: f.config.id.clone(),
                    path: f.config.path.clone(),
                    paused: f.engine.as_ref().is_some_and(SyncEngine::is_paused),
                    error: f.error.clone(),
                    engine: f
                        .engine
                        .as_ref()
                        .map(SyncEngine::status)
                        .unwrap_or_default(),
                    local_entries,
                    conflicts,
                }
            })
            .collect();
        let peers = self
            .config
            .devices
            .iter()
            .filter_map(|d| {
                let id = d.device_id().ok()?;
                let seen = self.peers_seen.get(&id);
                Some(PeerStatus {
                    id,
                    name: if d.name.is_empty() {
                        seen.map(|s| s.0.clone()).unwrap_or_default()
                    } else {
                        d.name.clone()
                    },
                    connected: self.net.is_connected(&id),
                    address: self.net.remote_address(&id),
                })
            })
            .collect();
        DaemonStatus {
            metered: self.metered(),
            device_id: Some(self.me),
            device_name: self.config.device_name.clone(),
            listen: self.net.local_addr().ok(),
            folders,
            peers,
        }
    }

    fn on_control(
        &mut self,
        c: Control,
        paired: &PairedSet,
        folders_cfg: &Arc<RwLock<Vec<FolderConfig>>>,
        watch: WatchConfig,
    ) {
        match c {
            Control::Status(reply) => {
                let _ = reply.send(self.status());
            }
            Control::Reload(reply) => {
                let _ = reply.send(
                    self.reload(paired, folders_cfg, watch)
                        .map_err(|e| format!("{e:#}")),
                );
            }
            Control::Pause(id, paused, reply) => {
                let r = match self.folders.iter_mut().find(|f| f.config.id == id) {
                    Some(f) => {
                        f.config.paused = paused;
                        if let Some(e) = f.engine.as_mut() {
                            e.set_paused(paused);
                        }
                        Ok(())
                    }
                    None => Err(format!("no folder {id:?}")),
                };
                let _ = reply.send(r);
            }
            Control::Rescan(id, reply) => {
                let r = match self.folders.iter_mut().find(|f| f.config.id == id) {
                    Some(f) => match f.engine.as_mut() {
                        Some(e) => e.full_scan().map_err(|e| e.to_string()),
                        None => Err(f.error.clone().unwrap_or_default()),
                    },
                    None => Err(format!("no folder {id:?}")),
                };
                let _ = reply.send(r);
            }
        }
    }

    /// Apply a changed config.json: pairing changes take effect for new
    /// connections immediately; folders are reopened if they changed.
    fn reload(
        &mut self,
        paired: &PairedSet,
        folders_cfg: &Arc<RwLock<Vec<FolderConfig>>>,
        watch: WatchConfig,
    ) -> Result<()> {
        let config = Config::load(&self.config_dir)?;
        let new_paired = config.paired();
        let removed: Vec<DeviceId> = self
            .config
            .paired()
            .difference(&new_paired)
            .copied()
            .collect();
        if let Ok(mut p) = paired.write() {
            *p = new_paired.clone();
        }
        for d in removed {
            self.net.disconnect(&d);
        }
        self.dialer.set_devices(config.dial_targets());
        let bandwidth_changed = config.bandwidth != self.config.bandwidth;
        if let Ok(mut f) = folders_cfg.write() {
            *f = config.folders.clone();
        }
        let old: Vec<FolderConfig> = self.folders.iter().map(|f| f.config.clone()).collect();
        self.config = config;
        if bandwidth_changed {
            self.apply_bandwidth();
        }
        if old != self.config.folders {
            // Keep folders whose config is unchanged; reopen the others.
            let mut previous = std::mem::take(&mut self.folders);
            for cfg in self.config.folders.clone() {
                let kept = previous
                    .iter()
                    .position(|f| f.config == cfg && f.engine.is_some())
                    .map(|i| previous.swap_remove(i));
                let f = match kept {
                    Some(f) => f,
                    None => {
                        // Close the old engine (and its index) before reopening.
                        previous.retain(|f| f.config.id != cfg.id);
                        self.open_folder(cfg, watch)
                    }
                };
                self.folders.push(f);
            }
            drop(previous);
            let peers = self.net.connected_peers();
            for f in &mut self.folders {
                if let Some(e) = f.engine.as_mut() {
                    e.set_shared_with(f.config.shared_with());
                    for p in &peers {
                        if !e.status().connected.contains(p) {
                            let _ = e.on_connected(*p);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
