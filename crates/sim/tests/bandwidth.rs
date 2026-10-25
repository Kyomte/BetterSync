//! M11: large files wait for an unmetered network, in both directions.

#![allow(clippy::unwrap_used)]

use bsync_fs::FileSystem;
use bsync_sim::Cluster;

fn engine(c: &mut Cluster, n: usize) -> &mut bsync_engine::SyncEngine {
    c.nodes[n].engine.as_mut().unwrap()
}

fn read(c: &Cluster, node: usize, rel: &str) -> Option<Vec<u8>> {
    c.nodes[node].fs.read(&Cluster::path(rel)).ok()
}

#[test]
fn large_files_wait_while_the_receiver_is_metered() {
    let mut c = Cluster::new(1, 2);
    for n in 0..2 {
        engine(&mut c, n).set_metered_max_file(Some(10_000));
    }
    engine(&mut c, 1).set_metered(true);
    let big = c.rng.bytes(50_000);
    c.nodes[0]
        .fs
        .write_in_place(&Cluster::path("big.bin"), &big)
        .unwrap();
    c.nodes[0]
        .fs
        .write_in_place(&Cluster::path("small.txt"), b"small")
        .unwrap();
    c.full_scan(0);
    assert!(c.settle(2000));
    assert_eq!(read(&c, 1, "small.txt").as_deref(), Some(&b"small"[..]));
    assert!(
        read(&c, 1, "big.bin").is_none(),
        "large file fetched on a metered network"
    );
    assert_eq!(
        engine(&mut c, 1).status().chunks_requested,
        1,
        "only the small file's chunk"
    );
    let blocked = engine(&mut c, 1).status().blocked;
    assert!(
        blocked
            .iter()
            .any(|(p, why)| p == "big.bin" && why.contains("unmetered")),
        "{blocked:?}"
    );

    engine(&mut c, 1).set_metered(false);
    assert!(c.settle(2000));
    assert_eq!(read(&c, 1, "big.bin").as_deref(), Some(&big[..]));
}

#[test]
fn a_metered_sender_does_not_upload_large_files() {
    let mut c = Cluster::new(2, 2);
    for n in 0..2 {
        engine(&mut c, n).set_metered_max_file(Some(10_000));
    }
    engine(&mut c, 0).set_metered(true);
    let big = c.rng.bytes(50_000);
    c.nodes[0]
        .fs
        .write_in_place(&Cluster::path("big.bin"), &big)
        .unwrap();
    c.full_scan(0);
    // The receiver keeps asking (and retrying); nothing large is sent.
    for _ in 0..10 {
        c.settle(200);
    }
    assert!(read(&c, 1, "big.bin").is_none());

    engine(&mut c, 0).set_metered(false);
    assert!(c.settle(5000));
    assert_eq!(read(&c, 1, "big.bin").as_deref(), Some(&big[..]));
}
