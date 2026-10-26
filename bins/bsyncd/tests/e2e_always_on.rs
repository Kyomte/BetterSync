//! S4: an always-on peer accepts the folders paired devices offer.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually, tree};

#[test]
fn server_accepts_offered_folders() {
    let laptop = Dev::new("laptop");
    let server = Dev::new("server");
    let root = server.tmp.path().join("srv");
    let photos = laptop.tmp.path().join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::write(photos.join("cat.jpg"), b"meow").unwrap();

    server.edit(|c| {
        c.local_discovery = false;
        c.pair(laptop.id, None, vec![]);
        c.always_on.auto_accept_folders = true;
        c.always_on.folders_root = Some(root.clone());
    });
    let ds = server.start();
    let addr = ds.local_addr().to_string();
    laptop.edit(|c| {
        c.local_discovery = false;
        c.pair(server.id, None, vec![addr]);
        c.add_folder("photos", photos.clone(), &[server.id])
            .unwrap();
    });
    let dl = laptop.start();
    eventually(30, "photos accepted and synced", || {
        tree(&root.join("photos")) == tree(&photos)
    });

    // A folder added on the laptop while connected is accepted too.
    let docs = laptop.tmp.path().join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("cv.pdf"), b"pdf").unwrap();
    laptop.edit(|c| c.add_folder("docs", docs.clone(), &[server.id]).unwrap());
    dl.reload().unwrap();
    eventually(30, "docs accepted and synced", || {
        tree(&root.join("docs")) == tree(&docs)
    });

    let cfg = bsyncd::Config::load(&server.config_dir()).unwrap();
    assert_eq!(cfg.folders.len(), 2);
    assert!(
        cfg.folders
            .iter()
            .all(|f| f.shared_with().contains(&laptop.id))
    );
}

#[test]
fn ordinary_peer_does_not_accept_folders() {
    let a = Dev::new("a");
    let b = Dev::new("b");
    b.edit(|c| {
        c.local_discovery = false;
        c.pair(a.id, None, vec![]);
    });
    let db = b.start();
    let addr = db.local_addr().to_string();
    a.edit(|c| {
        c.local_discovery = false;
        c.pair(b.id, None, vec![addr]);
        c.add_folder("f", a.folder(), &[b.id]).unwrap();
    });
    let da = a.start();
    eventually(20, "connected", || da.net().is_connected(&b.id));
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        bsyncd::Config::load(&b.config_dir())
            .unwrap()
            .folders
            .is_empty()
    );
}
