//! S2: concurrent edits of text files are merged when they touch different
//! lines; otherwise both versions are kept as before.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use bsync_sim::Cluster;

fn write(c: &Cluster, n: usize, rel: &str, data: &str) {
    c.nodes[n]
        .fs
        .write_in_place(&Cluster::path(rel), data.as_bytes())
        .unwrap();
}

fn files(c: &Cluster, n: usize) -> BTreeMap<String, String> {
    c.tree(n)
        .into_iter()
        .filter_map(|(k, v)| v.map(|d| (k, String::from_utf8_lossy(&d).into_owned())))
        .collect()
}

fn converge(c: &mut Cluster) {
    assert!(c.settle(5000));
    for i in 1..c.nodes.len() {
        assert_eq!(c.tree(0), c.tree(i), "node {i} differs");
    }
}

fn merged_count(c: &Cluster) -> u64 {
    c.nodes
        .iter()
        .map(|n| n.engine.as_ref().unwrap().status().totals.merged)
        .sum()
}

const BASE: &str = "title\nline 2\nline 3\nline 4\nline 5\nline 6\n";

/// Two nodes sharing `notes.txt` = BASE, then partitioned.
fn split(seed: u64) -> Cluster {
    let mut c = Cluster::new(seed, 2);
    write(&c, 0, "notes.txt", BASE);
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);
    c
}

fn heal(c: &mut Cluster) {
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    converge(c);
}

#[test]
fn edits_to_different_lines_merge() {
    let mut c = split(1);
    write(
        &c,
        0,
        "notes.txt",
        "TITLE\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    write(
        &c,
        1,
        "notes.txt",
        "title\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\n",
    );
    heal(&mut c);
    assert_eq!(
        files(&c, 0),
        BTreeMap::from([(
            "notes.txt".into(),
            "TITLE\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\n".into()
        )])
    );
    assert_eq!(merged_count(&c), 1);
}

#[test]
fn several_offline_edits_on_one_side_still_merge() {
    let mut c = split(2);
    write(
        &c,
        0,
        "notes.txt",
        "TITLE\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    c.full_scan(0);
    write(
        &c,
        0,
        "notes.txt",
        "TITLE!\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    c.full_scan(0);
    write(
        &c,
        1,
        "notes.txt",
        "title\nline 2\nline 3\nline 4\nline 5\nLINE 6\n",
    );
    heal(&mut c);
    assert_eq!(
        files(&c, 1)["notes.txt"],
        "TITLE!\nline 2\nline 3\nline 4\nline 5\nLINE 6\n"
    );
}

#[test]
fn edit_on_top_of_a_synced_edit_merges_with_a_concurrent_one() {
    let mut c = Cluster::new(3, 2);
    write(&c, 0, "notes.txt", BASE);
    c.full_scan(0);
    converge(&mut c);
    // Node 0 edits and syncs; then both edit concurrently on top of that.
    write(
        &c,
        0,
        "notes.txt",
        "TITLE\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);
    write(
        &c,
        0,
        "notes.txt",
        "TITLE\nline 2 (zero)\nline 3\nline 4\nline 5\nline 6\n",
    );
    write(
        &c,
        1,
        "notes.txt",
        "TITLE\nline 2\nline 3\nline 4\nline 5 (one)\nline 6\n",
    );
    heal(&mut c);
    assert_eq!(
        files(&c, 0)["notes.txt"],
        "TITLE\nline 2 (zero)\nline 3\nline 4\nline 5 (one)\nline 6\n"
    );
}

#[test]
fn overlapping_edits_keep_a_conflict_copy() {
    let mut c = split(4);
    write(
        &c,
        0,
        "notes.txt",
        "title\nline 2 from zero\nline 3\nline 4\nline 5\nline 6\n",
    );
    write(
        &c,
        1,
        "notes.txt",
        "title\nline 2 from one\nline 3\nline 4\nline 5\nline 6\n",
    );
    heal(&mut c);
    let f = files(&c, 0);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f.values().any(|v| v.contains("from zero")));
    assert!(f.values().any(|v| v.contains("from one")));
    assert_eq!(merged_count(&c), 0);
}

#[test]
fn binary_files_are_never_merged() {
    let mut c = Cluster::new(5, 2);
    let base = [0u8, 1, 2, 3, 4, 5, 6, 7];
    c.nodes[0]
        .fs
        .write_in_place(&Cluster::path("data.bin"), &base)
        .unwrap();
    c.full_scan(0);
    converge(&mut c);
    c.partition(0, 1);
    c.nodes[0]
        .fs
        .write_in_place(&Cluster::path("data.bin"), &[9, 1, 2, 3, 4, 5, 6, 7])
        .unwrap();
    c.nodes[1]
        .fs
        .write_in_place(&Cluster::path("data.bin"), &[0, 1, 2, 3, 4, 5, 6, 9])
        .unwrap();
    heal(&mut c);
    assert_eq!(c.tree(0).len(), 2, "kept both versions");
    assert_eq!(merged_count(&c), 0);
}

#[test]
fn three_devices_merge_to_the_same_text() {
    let mut c = Cluster::new(6, 3);
    write(&c, 0, "notes.txt", BASE);
    c.full_scan(0);
    converge(&mut c);
    for (a, b) in [(0, 1), (1, 2), (0, 2)] {
        c.partition(a, b);
    }
    write(
        &c,
        0,
        "notes.txt",
        "TITLE\nline 2\nline 3\nline 4\nline 5\nline 6\n",
    );
    write(
        &c,
        1,
        "notes.txt",
        "title\nline 2\nline 3 (one)\nline 4\nline 5\nline 6\n",
    );
    write(
        &c,
        2,
        "notes.txt",
        "title\nline 2\nline 3\nline 4\nline 5\nline 6 (two)\n",
    );
    for n in 0..3 {
        c.full_scan(n);
    }
    c.heal_all();
    converge(&mut c);
    assert_eq!(
        files(&c, 2)["notes.txt"],
        "TITLE\nline 2\nline 3 (one)\nline 4\nline 5\nline 6 (two)\n"
    );
}
