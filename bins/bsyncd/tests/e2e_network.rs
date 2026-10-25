//! M4: two daemons on localhost pair, exchange indexes and sync one way
//! over QUIC; an unpaired daemon is rejected.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually, tree};

#[test]
fn paired_daemons_sync_and_unpaired_is_rejected() {
    let a = Dev::new("a");
    let b = Dev::new("b");
    let c = Dev::new("c");

    std::fs::create_dir_all(a.folder().join("sub")).unwrap();
    std::fs::write(a.folder().join("hello.txt"), b"hello from a").unwrap();
    std::fs::write(a.folder().join("sub/data.bin"), vec![7u8; 300_000]).unwrap();

    // a knows b (but not c) and shares the folder with b.
    a.edit(|cfg| {
        cfg.pair(b.id, Some("b".into()), vec![]);
        cfg.add_folder("docs", a.folder(), &[b.id]).unwrap();
    });
    let da = a.start();
    let addr = da.local_addr().to_string();

    b.edit(|cfg| {
        cfg.pair(a.id, Some("a".into()), vec![addr.clone()]);
        cfg.add_folder("docs", b.folder(), &[a.id]).unwrap();
    });
    // c trusts a, but a does not trust c.
    c.edit(|cfg| {
        cfg.pair(a.id, Some("a".into()), vec![addr.clone()]);
        cfg.add_folder("docs", c.folder(), &[a.id]).unwrap();
    });
    let db = b.start();
    let dc = c.start();

    eventually(20, "b gets a's files", || {
        tree(&b.folder()) == tree(&a.folder())
    });

    // A change after startup travels too.
    std::fs::write(a.folder().join("hello.txt"), b"hello again").unwrap();
    eventually(20, "edit reaches b", || {
        std::fs::read(b.folder().join("hello.txt")).ok().as_deref() == Some(&b"hello again"[..])
    });

    let sa = da.status().unwrap();
    assert!(sa.peers.iter().any(|p| p.id == b.id && p.connected));
    assert!(!da.net().is_connected(&c.id));
    assert!(!dc.net().is_connected(&a.id));
    assert!(
        tree(&c.folder()).is_empty(),
        "unpaired device received data"
    );

    drop(dc);
    drop(db);
    drop(da);
}
