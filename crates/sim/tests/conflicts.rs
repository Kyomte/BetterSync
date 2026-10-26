//! M5: two-way sync and the OS-independent "nasty cases" from PLAN.md, on
//! the deterministic simulator.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bsync_fs::FileSystem;
use bsync_sim::Cluster;

fn write(c: &Cluster, node: usize, rel: &str, data: &[u8]) {
    c.nodes[node]
        .fs
        .write_in_place(&Cluster::path(rel), data)
        .unwrap();
}

fn mkdir(c: &Cluster, node: usize, rel: &str) {
    c.nodes[node].fs.create_dir(&Cluster::path(rel)).unwrap();
}

fn rm(c: &Cluster, node: usize, rel: &str) {
    c.nodes[node].fs.remove_file(&Cluster::path(rel)).unwrap();
}

fn files(t: &BTreeMap<String, Option<Arc<Vec<u8>>>>) -> BTreeMap<String, Vec<u8>> {
    t.iter()
        .filter_map(|(k, v)| v.as_ref().map(|d| (k.clone(), d.to_vec())))
        .collect()
}

fn contents(c: &Cluster, node: usize) -> Vec<Vec<u8>> {
    files(&c.tree(node)).into_values().collect()
}

fn converge(c: &mut Cluster) {
    assert!(c.settle(5000), "did not settle");
    for i in 1..c.nodes.len() {
        assert_eq!(c.tree(0), c.tree(i), "node {i} differs from node 0");
    }
}

/// Two nodes with `rel` synced to both, then partitioned.
fn split_pair(seed: u64, rel: &str, data: &[u8]) -> Cluster {
    let mut c = Cluster::new(seed, 2);
    write(&c, 0, rel, data);
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);
    c
}

#[test]
fn concurrent_edits_keep_both_versions() {
    let mut c = split_pair(10, "notes.txt", b"base");
    write(&c, 0, "notes.txt", b"edit on 0");
    write(&c, 1, "notes.txt", b"edit on 1");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(&mut c);

    let f = files(&c.tree(0));
    assert_eq!(f.len(), 2, "{f:?}");
    let all: Vec<_> = f.values().cloned().collect();
    assert!(all.contains(&b"edit on 0".to_vec()) && all.contains(&b"edit on 1".to_vec()));
    let copy = f.keys().find(|k| k.contains(".sync-conflict-")).unwrap();
    assert!(
        copy.starts_with("notes.sync-conflict-") && copy.ends_with(".txt"),
        "{copy}"
    );
}

#[test]
fn identical_concurrent_edits_do_not_conflict() {
    let mut c = split_pair(11, "same.txt", b"base");
    write(&c, 0, "same.txt", b"same result");
    write(&c, 1, "same.txt", b"same result");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(&mut c);
    assert_eq!(files(&c.tree(0)).len(), 1);
}

#[test]
fn edit_beats_delete_whichever_side_deletes() {
    for deleter in [0, 1] {
        let editor = 1 - deleter;
        let mut c = split_pair(20 + deleter as u64, "doc.txt", b"base");
        rm(&c, deleter, "doc.txt");
        write(&c, editor, "doc.txt", b"precious edit");
        c.full_scan(0);
        c.full_scan(1);
        c.heal(0, 1);
        converge(&mut c);
        assert_eq!(
            files(&c.tree(0)),
            BTreeMap::from([("doc.txt".to_string(), b"precious edit".to_vec())])
        );
    }
}

#[test]
fn directory_delete_vs_add_inside_recreates_dir_with_only_new_file() {
    let mut c = Cluster::new(30, 2);
    mkdir(&c, 0, "d");
    write(&c, 0, "d/old.txt", b"old");
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);

    // Node 0 deletes the whole directory; node 1 adds a file inside it.
    rm(&c, 0, "d/old.txt");
    c.nodes[0].fs.remove_dir(&Cluster::path("d")).unwrap();
    write(&c, 1, "d/new.txt", b"new");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(&mut c);
    assert_eq!(
        files(&c.tree(0)),
        BTreeMap::from([("d/new.txt".to_string(), b"new".to_vec())])
    );
}

#[test]
fn rename_is_detected_by_chunks_and_not_resent() {
    let mut c = Cluster::new(40, 2);
    let data = c.rng.bytes(64_000);
    write(&c, 0, "a.bin", &data);
    c.full_scan(0);
    converge(&mut c);
    let fetched_before = c.nodes[1]
        .engine
        .as_ref()
        .unwrap()
        .status()
        .totals
        .chunks_fetched;

    c.nodes[0]
        .fs
        .rename(&Cluster::path("a.bin"), &Cluster::path("b.bin"))
        .unwrap();
    c.full_scan(0);
    converge(&mut c);
    let status = c.nodes[1].engine.as_ref().unwrap().status();
    assert_eq!(
        status.totals.chunks_fetched, fetched_before,
        "renamed content was re-sent"
    );
    assert_eq!(files(&c.tree(1)).keys().collect::<Vec<_>>(), vec!["b.bin"]);
}

#[test]
fn file_vs_directory_conflict_keeps_both() {
    let mut c = Cluster::new(50, 2);
    c.partition(0, 1);
    write(&c, 0, "x", b"a file named x");
    mkdir(&c, 1, "x");
    write(&c, 1, "x/inside", b"inside the dir");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(&mut c);
    let t = c.tree(0);
    assert_eq!(t.get("x"), Some(&None), "directory wins the name: {t:?}");
    assert!(contents(&c, 0).contains(&b"a file named x".to_vec()));
    assert!(contents(&c, 0).contains(&b"inside the dir".to_vec()));
}

#[test]
fn concurrent_deletes_converge() {
    let mut c = split_pair(60, "gone.txt", b"bye");
    rm(&c, 0, "gone.txt");
    rm(&c, 1, "gone.txt");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(&mut c);
    assert!(files(&c.tree(0)).is_empty());
}

#[test]
fn three_way_conflict_through_a_relay() {
    // 0 and 2 never talk directly; 1 relays. All three edit concurrently.
    let mut c = Cluster::new(70, 3);
    write(&c, 0, "f", b"base");
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);
    c.partition(1, 2);
    c.partition(0, 2);
    for (i, text) in [b"zero", b"one!", b"two!"].iter().enumerate() {
        write(&c, i, "f", *text);
        c.full_scan(i);
    }
    c.heal(0, 1);
    c.heal(1, 2);
    converge(&mut c);
    let all = contents(&c, 0);
    for text in [&b"zero"[..], b"one!", b"two!"] {
        assert!(
            all.contains(&text.to_vec()),
            "lost {:?}: {all:?}",
            String::from_utf8_lossy(text)
        );
    }
}

#[test]
fn loser_offline_until_later_still_keeps_its_edit() {
    let mut c = Cluster::new(80, 3);
    write(&c, 0, "f", b"base");
    c.full_scan(0);
    converge(&mut c);
    c.stop(0);
    // Node 0 edits while its engine is down; nodes 1 and 2 edit too.
    write(&c, 0, "f", b"offline edit");
    write(&c, 2, "f", b"online edit");
    c.full_scan(2);
    assert!(c.settle(5000));
    assert_eq!(c.tree(1), c.tree(2));
    c.start(0);
    converge(&mut c);
    let all = contents(&c, 1);
    assert!(all.contains(&b"offline edit".to_vec()) && all.contains(&b"online edit".to_vec()));
}

#[test]
fn unscanned_local_edit_is_not_overwritten() {
    let mut c = split_pair(90, "f", b"base");
    write(&c, 0, "f", b"remote edit");
    c.full_scan(0);
    // Node 1 edits but its watcher has not reported it yet.
    write(&c, 1, "f", b"local unscanned edit");
    c.heal(0, 1);
    converge(&mut c);
    let all = contents(&c, 1);
    assert!(all.contains(&b"local unscanned edit".to_vec()), "{all:?}");
    assert!(all.contains(&b"remote edit".to_vec()), "{all:?}");
}
