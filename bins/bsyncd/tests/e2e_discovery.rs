//! M7: two daemons on the same machine find each other via mDNS (no
//! addresses configured) and sync. Needs working multicast; set
//! `BSYNC_SKIP_MDNS=1` where it is unavailable.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually, tree};

#[test]
fn daemons_find_each_other_via_mdns() {
    if std::env::var_os("BSYNC_SKIP_MDNS").is_some() {
        eprintln!("skipped: BSYNC_SKIP_MDNS is set");
        return;
    }
    let a = Dev::new("a");
    let b = Dev::new("b");
    for (me, other) in [(&a, &b), (&b, &a)] {
        me.edit(|c| {
            c.listen = "0.0.0.0:0".into();
            c.pair(other.id, None, vec![]); // no address: discovery only
            c.add_folder("f", me.folder(), &[other.id]).unwrap();
        });
    }
    std::fs::write(a.folder().join("found.txt"), b"via mdns").unwrap();
    let da = a.start();
    let db = b.start();
    eventually(30, "mDNS discovery + sync", || {
        tree(&b.folder()) == tree(&a.folder())
    });
    assert!(da.net().is_connected(&b.id) || db.net().is_connected(&a.id));
}
