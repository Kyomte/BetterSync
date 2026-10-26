//! S1 (Windows): with Explorer badges on, files become Cloud Files
//! placeholders, and syncing still works through creates, edits, deletes
//! and replaces.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually, tree};

#[test]
fn syncing_works_with_explorer_badges() {
    let a = Dev::new("a");
    let b = Dev::new("b");
    std::fs::create_dir_all(a.folder().join("sub")).unwrap();
    std::fs::write(a.folder().join("sub/one.txt"), b"one").unwrap();
    std::fs::write(a.folder().join("two.bin"), vec![3u8; 200_000]).unwrap();

    a.edit(|cfg| {
        cfg.explorer_badges = true;
        cfg.pair(b.id, Some("b".into()), vec![]);
        cfg.add_folder("docs", a.folder(), &[b.id]).unwrap();
    });
    let da = a.start();
    let addr = da.local_addr().to_string();
    b.edit(|cfg| {
        cfg.explorer_badges = true;
        cfg.pair(a.id, Some("a".into()), vec![addr]);
        cfg.add_folder("docs", b.folder(), &[a.id]).unwrap();
    });
    let db = b.start();

    eventually(30, "initial sync", || {
        tree(&b.folder()) == tree(&a.folder())
    });
    let one_b = b.folder().join("sub/one.txt");
    eventually(20, "synced badge state", || {
        db.path_status(vec![one_b.clone()]).unwrap()[0].state == "synced"
    });

    // Edits in both directions replace files that are now placeholders.
    std::fs::write(a.folder().join("sub/one.txt"), b"one, edited on a").unwrap();
    eventually(20, "edit a -> b", || {
        std::fs::read(&one_b).ok().as_deref() == Some(&b"one, edited on a"[..])
    });
    std::fs::write(&one_b, b"edited again on b").unwrap();
    eventually(20, "edit b -> a", || {
        std::fs::read(a.folder().join("sub/one.txt"))
            .ok()
            .as_deref()
            == Some(&b"edited again on b"[..])
    });
    std::fs::remove_file(b.folder().join("two.bin")).unwrap();
    eventually(20, "delete b -> a", || !a.folder().join("two.bin").exists());
    eventually(20, "trees equal", || tree(&b.folder()) == tree(&a.folder()));

    drop(db);
    drop(da);
    for d in [&a, &b] {
        // Fails harmlessly if registration was not possible on this machine.
        let _ = bsync_cloudfiles::SyncRoot::unregister(&d.folder());
    }
}
