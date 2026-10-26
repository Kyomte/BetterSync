//! S1: the per-path state file manager badges show.

#![allow(clippy::unwrap_used)]

use bsync_engine::{PathState, SyncEngine};
use bsync_fs::FileSystem;
use bsync_sim::Cluster;

fn write(c: &Cluster, node: usize, rel: &str, data: &[u8]) {
    c.nodes[node]
        .fs
        .write_in_place(&Cluster::path(rel), data)
        .unwrap();
}

fn engine(c: &Cluster, node: usize) -> &SyncEngine {
    c.nodes[node].engine.as_ref().unwrap()
}

fn state(c: &Cluster, node: usize, rel: &str) -> PathState {
    engine(c, node).path_state(rel).unwrap()
}

#[test]
fn files_and_folders_report_their_sync_state() {
    let mut c = Cluster::new(1, 2);
    c.nodes[0].fs.create_dir(&Cluster::path("docs")).unwrap();
    write(&c, 0, "docs/a.txt", b"hello");
    // On disk, not scanned yet.
    assert_eq!(state(&c, 0, "docs/a.txt"), PathState::Syncing);
    assert_eq!(state(&c, 0, "docs"), PathState::Syncing);

    c.full_scan(0);
    assert!(c.settle(2000));
    for node in 0..2 {
        assert_eq!(state(&c, node, "docs/a.txt"), PathState::Synced);
        assert_eq!(state(&c, node, "docs"), PathState::Synced);
        assert_eq!(state(&c, node, ""), PathState::Synced);
        assert_eq!(state(&c, node, "/docs/"), PathState::Synced);
    }
    assert_eq!(state(&c, 0, "missing.txt"), PathState::Unknown);
    assert_eq!(state(&c, 0, ".bsync"), PathState::Ignored);

    // Edited but not scanned.
    write(&c, 1, "docs/a.txt", b"hello, world");
    assert_eq!(state(&c, 1, "docs/a.txt"), PathState::Syncing);
    c.notify_change(1, &["docs/a.txt".into()]);
    assert_eq!(state(&c, 1, "docs"), PathState::Syncing);
    // Deleted but not scanned.
    c.nodes[0]
        .fs
        .remove_file(&Cluster::path("docs/a.txt"))
        .unwrap();
    assert_eq!(state(&c, 0, "docs/a.txt"), PathState::Syncing);
}

#[test]
fn ignored_paths_are_reported() {
    let mut c = Cluster::new(2, 1);
    write(&c, 0, ".bsyncignore", b"*.log\n");
    write(&c, 0, "debug.log", b"noise");
    c.full_scan(0);
    assert!(c.settle(1000));
    assert_eq!(state(&c, 0, "debug.log"), PathState::Ignored);
    assert_eq!(state(&c, 0, ""), PathState::Synced);
}

#[test]
fn conflicts_and_blocked_paths_show_up_on_their_folders() {
    let mut c = Cluster::new(3, 2);
    c.nodes[0].fs.create_dir(&Cluster::path("d")).unwrap();
    write(&c, 0, "d/notes.bin", &[0xFF, 1]);
    write(&c, 0, "doc.txt", b"v1");
    c.full_scan(0);
    assert!(c.settle(2000));

    // Concurrent binary edits: a conflict copy appears.
    c.partition(0, 1);
    write(&c, 0, "d/notes.bin", &[0xFF, 2]);
    write(&c, 1, "d/notes.bin", &[0xFF, 3]);
    c.full_scan(0);
    c.full_scan(1);
    c.heal(0, 1);
    assert!(c.settle(5000));
    let copy = c
        .tree(0)
        .into_keys()
        .find(|k| k.contains(".sync-conflict-"))
        .unwrap();
    assert_eq!(state(&c, 0, &copy), PathState::Conflict);
    assert_eq!(state(&c, 0, "d"), PathState::Conflict);
    assert_eq!(state(&c, 0, ""), PathState::Conflict);
    assert_eq!(state(&c, 0, "doc.txt"), PathState::Synced);

    // A file locked by another program is blocked on the receiving side.
    c.nodes[1].fs.set_locked(&Cluster::path("doc.txt"), true);
    write(&c, 0, "doc.txt", b"v2");
    c.full_scan(0);
    for _ in 0..50 {
        c.tick_all();
        while c.deliver_one() {}
    }
    assert!(matches!(state(&c, 1, "doc.txt"), PathState::Blocked(_)));
    assert_eq!(state(&c, 0, "doc.txt"), PathState::Synced);
}

#[test]
fn touched_paths_cover_every_change() {
    let mut c = Cluster::new(4, 2);
    write(&c, 0, "a.txt", b"a");
    write(&c, 0, "b.txt", b"b");
    c.full_scan(0);
    assert!(c.settle(2000));

    let first = c.nodes[1].engine.as_mut().unwrap().take_touched().unwrap();
    assert!(
        first.contains("a.txt") && first.contains("b.txt"),
        "{first:?}"
    );
    let again = c.nodes[1].engine.as_mut().unwrap().take_touched().unwrap();
    assert!(again.is_empty(), "{again:?}");

    write(&c, 0, "b.txt", b"b2");
    c.full_scan(0);
    assert!(c.settle(2000));
    let changed = c.nodes[1].engine.as_mut().unwrap().take_touched().unwrap();
    assert!(changed.contains("b.txt"), "{changed:?}");
    assert!(!changed.contains("a.txt"), "{changed:?}");
}
