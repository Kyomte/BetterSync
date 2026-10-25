//! M8: ignore rules, Unicode and case collisions, Windows names, locked
//! files, resumable transfers.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use bsync_fs::FileSystem;
use bsync_sim::Cluster;

fn write(c: &Cluster, node: usize, rel: &str, data: &[u8]) {
    c.nodes[node]
        .fs
        .write_in_place(&Cluster::path(rel), data)
        .unwrap();
}

fn read(c: &Cluster, node: usize, rel: &str) -> Option<Vec<u8>> {
    c.nodes[node].fs.read(&Cluster::path(rel)).ok()
}

fn blocked(c: &Cluster, node: usize) -> Vec<(String, String)> {
    c.nodes[node].engine.as_ref().unwrap().status().blocked
}

#[test]
fn bsyncignore_keeps_matching_paths_local() {
    let mut c = Cluster::new(1, 2);
    write(&c, 0, ".bsyncignore", b"*.tmp\n/build/\n");
    write(&c, 0, "keep.txt", b"keep");
    write(&c, 0, "scratch.tmp", b"local only");
    c.nodes[0].fs.create_dir(&Cluster::path("build")).unwrap();
    write(&c, 0, "build/out.o", b"object");
    c.full_scan(0);
    assert!(c.settle(2000));
    assert_eq!(read(&c, 1, "keep.txt").as_deref(), Some(&b"keep"[..]));
    assert!(
        read(&c, 1, ".bsyncignore").is_some(),
        "the rules themselves sync"
    );
    assert!(read(&c, 1, "scratch.tmp").is_none());
    assert!(read(&c, 1, "build/out.o").is_none());

    // Node 1 now has the rules too: its own .tmp files stay local.
    write(&c, 1, "mine.tmp", b"also local");
    c.full_scan(1);
    assert!(c.settle(2000));
    assert!(read(&c, 0, "mine.tmp").is_none());

    // Dropping a rule makes those files sync.
    write(&c, 0, ".bsyncignore", b"/build/\n");
    c.full_scan(0);
    assert!(c.settle(2000));
    c.full_scan(1);
    assert!(c.settle(2000));
    assert_eq!(
        read(&c, 1, "scratch.tmp").as_deref(),
        Some(&b"local only"[..])
    );
    assert_eq!(read(&c, 0, "mine.tmp").as_deref(), Some(&b"also local"[..]));
    assert!(read(&c, 1, "build/out.o").is_none());
}

#[test]
fn case_only_collision_is_refused_and_reported() {
    let mut c = Cluster::new(2, 2);
    c.partition(0, 1);
    write(&c, 0, "Readme.md", b"from 0");
    write(&c, 1, "README.md", b"from 1");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    assert!(c.settle(2000));
    // Neither side creates the other's name next to its own.
    assert!(read(&c, 0, "README.md").is_none());
    assert!(read(&c, 1, "Readme.md").is_none());
    assert_eq!(read(&c, 0, "Readme.md").as_deref(), Some(&b"from 0"[..]));
    for n in 0..2 {
        let b = blocked(&c, n);
        assert!(
            b.iter().any(|(_, why)| why.contains("only in case")),
            "node {n}: {b:?}"
        );
    }
}

#[test]
fn case_collision_through_a_parent_directory() {
    let mut c = Cluster::new(3, 2);
    c.partition(0, 1);
    c.nodes[0].fs.create_dir(&Cluster::path("Photos")).unwrap();
    write(&c, 0, "Photos/a.jpg", b"a");
    c.nodes[1].fs.create_dir(&Cluster::path("photos")).unwrap();
    write(&c, 1, "photos/b.jpg", b"b");
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    assert!(c.settle(2000));
    assert!(read(&c, 0, "photos/b.jpg").is_none());
    assert!(!blocked(&c, 0).is_empty());
}

#[test]
fn unicode_forms_are_one_file() {
    let mut c = Cluster::new(4, 2);
    let nfd = "cafe\u{301}.txt";
    let nfc = "caf\u{e9}.txt";
    write(&c, 0, nfd, b"v1");
    c.full_scan(0);
    assert!(c.settle(2000));
    assert_eq!(
        read(&c, 1, nfc).as_deref(),
        Some(&b"v1"[..]),
        "records use NFC"
    );

    // An edit on the NFC side updates the NFD file on node 0 in place.
    write(&c, 1, nfc, b"v2");
    c.full_scan(1);
    assert!(c.settle(2000));
    assert_eq!(read(&c, 0, nfd).as_deref(), Some(&b"v2"[..]));
    assert!(
        read(&c, 0, nfc).is_none(),
        "no second file in the other form"
    );
    assert_eq!(c.tree(0).len(), 1);
}

#[test]
fn names_windows_cannot_store_are_refused_there() {
    let mut c = Cluster::new(5, 2);
    c.nodes[1]
        .engine
        .as_mut()
        .unwrap()
        .folder_mut()
        .set_windows_rules(true);
    write(&c, 0, "CON.txt", b"device name");
    write(&c, 0, "what?.txt", b"bad char");
    write(&c, 0, "fine.txt", b"ok");
    c.full_scan(0);
    assert!(c.settle(2000));
    assert_eq!(read(&c, 1, "fine.txt").as_deref(), Some(&b"ok"[..]));
    assert!(read(&c, 1, "CON.txt").is_none());
    assert!(read(&c, 1, "what?.txt").is_none());
    assert_eq!(blocked(&c, 1).len(), 2, "{:?}", blocked(&c, 1));
}

#[test]
fn locked_file_is_retried_until_unlocked() {
    let mut c = Cluster::new(6, 2);
    write(&c, 0, "doc.txt", b"v1");
    c.full_scan(0);
    assert!(c.settle(2000));

    c.nodes[1].fs.set_locked(&Cluster::path("doc.txt"), true);
    write(&c, 0, "doc.txt", b"v2");
    c.full_scan(0);
    for _ in 0..50 {
        c.tick_all();
        while c.deliver_one() {}
    }
    assert_eq!(read(&c, 1, "doc.txt").as_deref(), Some(&b"v1"[..]));
    assert!(!blocked(&c, 1).is_empty());
    assert!(
        !c.nodes[1].engine.as_ref().unwrap().is_idle(),
        "a retry is scheduled"
    );

    c.nodes[1].fs.set_locked(&Cluster::path("doc.txt"), false);
    c.clock.advance(Duration::from_secs(600));
    assert!(c.settle(2000));
    assert_eq!(read(&c, 1, "doc.txt").as_deref(), Some(&b"v2"[..]));
    assert!(blocked(&c, 1).is_empty());
}

#[test]
fn interrupted_transfer_resumes_without_refetching() {
    let mut c = Cluster::new(7, 2);
    let data = c.rng.bytes(100_000);
    write(&c, 0, "big.bin", &data);
    c.full_scan(0);
    // Let part of the transfer happen.
    for _ in 0..30 {
        c.tick_all();
        for _ in 0..4 {
            c.deliver_one();
        }
    }
    let first = c.nodes[1]
        .engine
        .as_ref()
        .unwrap()
        .status()
        .chunks_requested;
    let cached = c.nodes[1]
        .fs
        .read_dir(&Cluster::path(".bsync/chunks"))
        .map(|l| l.len())
        .unwrap_or(0);
    assert!(cached > 0, "fetched chunks are on disk");
    assert!(
        read(&c, 1, "big.bin").is_none(),
        "transfer still incomplete"
    );

    // The receiver restarts mid-transfer.
    c.stop(1);
    c.start(1);
    assert!(c.settle(5000));
    assert_eq!(read(&c, 1, "big.bin").as_deref(), Some(&data[..]));
    let second = c.nodes[1]
        .engine
        .as_ref()
        .unwrap()
        .status()
        .chunks_requested;
    let total_chunks = c.nodes[0]
        .engine
        .as_ref()
        .unwrap()
        .folder()
        .index()
        .get("big.bin")
        .unwrap()
        .unwrap()
        .record
        .chunks
        .len() as u64;
    assert!(first > 0);
    assert!(
        second == total_chunks - cached as u64,
        "re-fetched cached chunks: {second} requested after restart, {cached} were cached, {total_chunks} total"
    );
}
