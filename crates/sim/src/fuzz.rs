//! Randomized whole-system runs.
//!
//! Each run uses one seed to drive 3–5 engines through random file
//! operations, partitions, reconnects and restarts. Then everything is
//! healed and left to settle, and the run checks:
//!
//! * every folder is identical;
//! * no edit was lost: every content a user ever wrote is still present
//!   (as some file, e.g. a conflict copy) unless a user later replaced or
//!   deleted it on a device where they could see it;
//! * no temp files are left behind, every engine is idle, and a rescan
//!   finds nothing new (the index matches the disk).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bsync_fs::FileSystem;

use crate::{Cluster, SimRng};

#[derive(Debug, Clone, Copy)]
pub struct FuzzConfig {
    pub min_nodes: usize,
    pub max_nodes: usize,
    pub steps: usize,
}

impl Default for FuzzConfig {
    fn default() -> Self {
        Self {
            min_nodes: 3,
            max_nodes: 5,
            steps: 300,
        }
    }
}

/// A small name space so devices collide on the same paths often.
const DIRS: [&str; 3] = ["", "a", "a/b"];
const NAMES: [&str; 4] = ["x.txt", "y.txt", "z", "w.bin"];

struct Run {
    c: Cluster,
    /// Every content ever written by a user op.
    written: BTreeSet<Vec<u8>>,
    /// Contents a user replaced or deleted while looking at them.
    superseded: BTreeSet<Vec<u8>>,
    counter: u64,
    seed: u64,
    log: Vec<String>,
}

fn files_of(t: &BTreeMap<String, Option<Arc<Vec<u8>>>>) -> Vec<(String, Arc<Vec<u8>>)> {
    t.iter()
        .filter_map(|(k, v)| v.as_ref().map(|d| (k.clone(), Arc::clone(d))))
        .collect()
}

impl Run {
    fn op(&mut self, what: String) {
        tracing::debug!(target: "fuzz", "OP {what}");
        self.log.push(what);
    }

    fn fs(&self, n: usize) -> &crate::MemFs {
        &self.c.nodes[n].fs
    }

    fn content(&mut self) -> Vec<u8> {
        self.counter += 1;
        let mut data = format!("seed {} write {}\n", self.seed, self.counter).into_bytes();
        let extra = match self.c.rng.below(10) {
            0 => 0,
            1..=6 => self.c.rng.index(200),
            _ => self.c.rng.index(4000),
        };
        data.extend(self.c.rng.bytes(extra));
        self.written.insert(data.clone());
        data
    }

    fn random_path(&mut self) -> String {
        let dir = DIRS[self.c.rng.index(DIRS.len())];
        let name = NAMES[self.c.rng.index(NAMES.len())];
        if dir.is_empty() {
            name.to_owned()
        } else {
            format!("{dir}/{name}")
        }
    }

    /// A file that exists on node `n`, if any.
    fn existing_file(&mut self, n: usize) -> Option<(String, Arc<Vec<u8>>)> {
        let files = files_of(&self.c.tree(n));
        if files.is_empty() {
            return None;
        }
        let i = self.c.rng.index(files.len());
        Some(files[i].clone())
    }

    fn mkdirs(&self, n: usize, rel: &str) -> bool {
        let mut cur = String::new();
        for part in rel
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .map_or(&[][..], |(_, d)| d)
        {
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(part);
            match self.fs(n).metadata(&Cluster::path(&cur)) {
                Ok(m) if m.kind == bsync_fs::FileKind::Dir => {}
                Ok(_) => return false, // a file is in the way
                Err(_) => {
                    if self.fs(n).create_dir(&Cluster::path(&cur)).is_err() {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// What a user does on node `n`. Returns the paths touched.
    fn user_op(&mut self, n: usize) -> Vec<String> {
        match self.c.rng.below(100) {
            // Create or overwrite a file.
            0..=44 => {
                let rel = match self.c.rng.below(3) {
                    0 => self.existing_file(n).map(|(p, _)| p),
                    _ => None,
                }
                .unwrap_or_else(|| self.random_path());
                if !self.mkdirs(n, &rel) {
                    return vec![];
                }
                let path = Cluster::path(&rel);
                let old = self.fs(n).read(&path).ok();
                let data = self.content();
                if self.fs(n).write_in_place(&path, &data).is_ok() {
                    self.superseded.extend(old);
                    self.op(format!("n{n} write {rel} #{}", self.counter));
                    vec![rel]
                } else {
                    vec![]
                }
            }
            // Delete a file.
            45..=59 => {
                let Some((rel, data)) = self.existing_file(n) else {
                    return vec![];
                };
                if self.fs(n).remove_file(&Cluster::path(&rel)).is_ok() {
                    self.superseded.insert(data.to_vec());
                    self.op(format!("n{n} rm {rel}"));
                    vec![rel]
                } else {
                    vec![]
                }
            }
            // Rename a file (possibly over another one).
            60..=74 => {
                let Some((from, _)) = self.existing_file(n) else {
                    return vec![];
                };
                let to = self.random_path();
                if to == from || !self.mkdirs(n, &to) {
                    return vec![];
                }
                let replaced = self.fs(n).read(&Cluster::path(&to)).ok();
                if self
                    .fs(n)
                    .rename(&Cluster::path(&from), &Cluster::path(&to))
                    .is_ok()
                {
                    self.superseded.extend(replaced);
                    self.op(format!("n{n} mv {from} {to}"));
                    vec![from, to]
                } else {
                    vec![]
                }
            }
            // Delete a whole directory tree.
            75..=79 => {
                let dir = DIRS[1 + self.c.rng.index(DIRS.len() - 1)];
                let t = self.c.tree(n);
                let prefix = format!("{dir}/");
                let inside: Vec<_> = t
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if !t.contains_key(dir) {
                    return vec![];
                }
                // Remove deepest first.
                for (k, v) in inside.iter().rev() {
                    let p = Cluster::path(k);
                    let ok = if v.is_some() {
                        self.fs(n).remove_file(&p).is_ok()
                    } else {
                        self.fs(n).remove_dir(&p).is_ok()
                    };
                    if ok && let Some(d) = v {
                        self.superseded.insert(d.to_vec());
                    }
                }
                let _ = self.fs(n).remove_dir(&Cluster::path(dir));
                self.op(format!("n{n} rm -r {dir}"));
                vec![dir.to_owned()]
            }
            // Create an empty directory.
            _ => {
                let dir = DIRS[1 + self.c.rng.index(DIRS.len() - 1)];
                if self.mkdirs(n, &format!("{dir}/_")) {
                    self.op(format!("n{n} mkdir {dir}"));
                    vec![dir.to_owned()]
                } else {
                    vec![]
                }
            }
        }
    }

    fn step(&mut self) {
        let nodes = self.c.nodes.len();
        let n = self.c.rng.index(nodes);
        match self.c.rng.below(100) {
            0..=29 => {
                let paths = self.user_op(n);
                // Usually the watcher reports the change; sometimes it is
                // missed until a later full scan.
                if !paths.is_empty() && self.c.rng.chance(85) {
                    self.c.notify_change(n, &paths);
                }
            }
            30..=69 => {
                for _ in 0..self.c.rng.below(20) {
                    self.c.deliver_one();
                }
            }
            70..=84 => self.c.tick(n),
            85..=88 => self.c.full_scan(n),
            89..=92 => {
                let m = self.c.rng.index(nodes);
                if m != n {
                    self.op(format!("partition n{n} n{m}"));
                    self.c.partition(n, m);
                }
            }
            93..=96 => {
                let m = self.c.rng.index(nodes);
                if m != n {
                    self.op(format!("heal n{n} n{m}"));
                    self.c.heal(n, m);
                }
            }
            97 => {
                if self.c.nodes[n].engine.is_some() {
                    self.op(format!("stop n{n}"));
                    self.c.stop(n);
                } else {
                    self.op(format!("start n{n}"));
                    self.c.start(n);
                }
            }
            _ => self
                .c
                .clock
                .advance(Duration::from_secs(1 + self.c.rng.below(40))),
        }
    }

    fn check(&mut self) -> Result<(), String> {
        let nodes = self.c.nodes.len();
        for n in 0..nodes {
            self.c.start(n);
        }
        self.c.heal_all();
        for n in 0..nodes {
            self.c.full_scan(n);
        }
        if !self.c.settle(20_000) {
            return Err("did not settle".into());
        }
        // A second round: a full scan must find nothing new.
        for n in 0..nodes {
            let changed = self.c.nodes[n]
                .engine
                .as_mut()
                .map(|e| e.folder_mut().scan("").map(|s| s.changed))
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            if !changed.is_empty() {
                return Err(format!(
                    "node {n}: index out of date with disk: {changed:?}"
                ));
            }
        }
        let t0 = self.c.tree(0);
        for n in 1..nodes {
            let tn = self.c.tree(n);
            if tn != t0 {
                let diff: Vec<_> = t0
                    .keys()
                    .chain(tn.keys())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter(|k| t0.get(*k) != tn.get(*k))
                    .collect();
                let mut detail = String::new();
                for path in &diff {
                    for (i, node) in self.c.nodes.iter().enumerate() {
                        let disk = self.c.tree(i).get(*path).cloned().map(|d| {
                            d.map(|d| String::from_utf8_lossy(&d[..d.len().min(24)]).into_owned())
                        });
                        let Some(e) = &node.engine else { continue };
                        let local = e.folder().index().get(path).ok().flatten();
                        let remotes = e.folder().index().remote(path).unwrap_or_default();
                        let plan = e.folder().plan(path).ok().flatten();
                        detail += &format!(
                            "\n  {path} @n{i}: disk={disk:?}\n    local={:?}\n    remotes={:?}\n    plan={:?}",
                            local.map(|l| summarize(&l.record)),
                            remotes
                                .iter()
                                .map(|(_, r)| summarize(r))
                                .collect::<Vec<_>>(),
                            plan.map(|a| summarize(a.record())),
                        );
                    }
                }
                return Err(format!("node {n} differs from node 0 at {diff:?}{detail}"));
            }
        }
        for k in t0.keys() {
            if k.split('/').any(|c| c.starts_with(bsync_fs::TEMP_PREFIX)) {
                return Err(format!("temp file left behind: {k}"));
            }
        }
        let present: BTreeSet<Vec<u8>> =
            files_of(&t0).into_iter().map(|(_, d)| d.to_vec()).collect();
        for w in self.written.difference(&self.superseded) {
            if !present.contains(w) {
                let head = String::from_utf8_lossy(&w[..w.len().min(24)]).into_owned();
                let hash = bsync_core::Hash::of(w);
                let mut detail = String::new();
                for (i, node) in self.c.nodes.iter().enumerate() {
                    let in_versions = node
                        .fs
                        .snapshot(&Cluster::path(".bsync/versions"))
                        .map(|t| t.values().flatten().any(|d| d.as_slice() == w.as_slice()))
                        .unwrap_or(false);
                    detail += &format!("\n  n{i}: in versions: {in_versions}");
                    let Some(e) = &node.engine else { continue };
                    let idx = e.folder().index();
                    for l in idx.all().unwrap_or_default() {
                        let path = l.record.path.clone();
                        let local_has = l.record.hash == hash;
                        let remote: Vec<_> = idx
                            .remote(&path)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|(_, r)| r.hash == hash)
                            .map(|(p, r)| {
                                format!("from n{}: {}", p.as_bytes()[0] - 1, summarize(&r))
                            })
                            .collect();
                        if local_has || !remote.is_empty() || remote_mentions(idx, &path, &hash) {
                            detail += &format!(
                                "\n    {path}: local={} remotes(with it)={remote:?}",
                                summarize(&l.record)
                            );
                        }
                    }
                }
                return Err(format!("lost edit: {head:?} {hash:?}{detail}"));
            }
        }
        for (n, node) in self.c.nodes.iter().enumerate() {
            if let Some(e) = &node.engine {
                let s = e.status();
                if s.pending != 0 || !s.blocked.is_empty() {
                    return Err(format!("node {n} not idle: {s:?}"));
                }
            }
        }
        Ok(())
    }
}

/// Run one seed. On failure the error includes the tail of the op log.
pub fn run(seed: u64, config: FuzzConfig) -> Result<(), String> {
    let mut setup = SimRng::new(seed ^ 0x5EED);
    let span = (config.max_nodes - config.min_nodes + 1) as u64;
    let nodes = config.min_nodes + setup.below(span) as usize;
    let mut run = Run {
        c: Cluster::new(seed, nodes),
        written: BTreeSet::new(),
        superseded: BTreeSet::new(),
        counter: 0,
        seed,
        log: Vec::new(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for _ in 0..config.steps {
            run.step();
        }
        run.check()
    }))
    .unwrap_or_else(|p| {
        Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "panic".into()))
    });
    result.map_err(|e| {
        let keep = std::env::var("BSYNC_FUZZ_LOG").map_or(40, |v| v.parse().unwrap_or(40));
        let tail: Vec<_> = run.log.iter().rev().take(keep).rev().cloned().collect();
        format!("seed {seed}: {e}\nlast ops:\n  {}", tail.join("\n  "))
    })
}

fn summarize(r: &bsync_core::FileRecord) -> String {
    let v: Vec<String> = r
        .version
        .iter()
        .map(|(d, c)| format!("{}:{c}", d.as_bytes()[0] - 1))
        .collect();
    format!(
        "{}{} v[{}] by{} h{}",
        if r.deleted { "DEL " } else { "" },
        match r.kind {
            bsync_core::EntryKind::Dir => "dir",
            bsync_core::EntryKind::File => "file",
        },
        v.join(","),
        r.modified_by.as_bytes()[0] - 1,
        &r.hash.to_hex()[..6]
    )
}

fn remote_mentions(idx: &bsync_index::Index, path: &str, hash: &bsync_core::Hash) -> bool {
    idx.remote(path)
        .unwrap_or_default()
        .iter()
        .any(|(_, r)| r.hash == *hash)
}
