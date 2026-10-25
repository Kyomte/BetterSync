//! SyncEngine over the simulated network.

#![allow(clippy::unwrap_used)]

use bsync_fs::FileSystem;
use bsync_sim::Cluster;

#[test]
fn one_way_sync_over_network() {
    let mut c = Cluster::new(1, 2);
    let fs = c.nodes[0].fs.clone();
    fs.create_dir(&Cluster::path("d")).unwrap();
    fs.write_in_place(&Cluster::path("d/a.txt"), b"hello")
        .unwrap();
    fs.write_in_place(&Cluster::path("big"), &c.rng.bytes(50_000))
        .unwrap();
    c.full_scan(0);
    assert!(c.settle(1000));
    assert_eq!(c.tree(0), c.tree(1));
    assert_eq!(c.tree(1).len(), 3);

    fs.write_in_place(&Cluster::path("d/a.txt"), b"hello again")
        .unwrap();
    fs.remove_file(&Cluster::path("big")).unwrap();
    c.notify_change(0, &["d/a.txt".into(), "big".into()]);
    assert!(c.settle(1000));
    assert_eq!(c.tree(0), c.tree(1));
}

#[test]
fn resumes_after_disconnect_and_restart() {
    let mut c = Cluster::new(2, 2);
    let fs = c.nodes[0].fs.clone();
    let data = c.rng.bytes(200_000);
    fs.write_in_place(&Cluster::path("big"), &data).unwrap();
    c.full_scan(0);
    // Let a few messages through, then cut the link mid-transfer.
    c.tick_all();
    for _ in 0..40 {
        c.deliver_one();
        c.tick_all();
    }
    c.partition(0, 1);
    c.stop(1);
    c.start(1);
    c.heal(0, 1);
    assert!(c.settle(2000));
    assert_eq!(c.tree(0), c.tree(1));
}
