//! M5: two daemons converge under concurrent edits.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually, tree};

fn setup() -> (Dev, Dev) {
    let a = Dev::new("a");
    let b = Dev::new("b");
    a.edit(|c| {
        c.pair(b.id, None, vec![]);
        c.add_folder("f", a.folder(), &[b.id]).unwrap();
    });
    b.edit(|c| {
        c.pair(a.id, None, vec![]);
        c.add_folder("f", b.folder(), &[a.id]).unwrap();
    });
    (a, b)
}

fn connect(a: &Dev, b: &Dev, da: &bsyncd::Daemon) {
    let addr = da.local_addr().to_string();
    b.edit(|c| c.pair(a.id, None, vec![addr]));
}

#[test]
fn concurrent_edits_converge_with_conflict_copies() {
    let (a, b) = setup();
    std::fs::write(a.folder().join("shared.txt"), b"base").unwrap();
    let da = a.start();
    connect(&a, &b, &da);
    let db = b.start();
    eventually(20, "initial sync", || {
        tree(&a.folder()) == tree(&b.folder())
    });

    // b goes offline; both edit the same file and different files.
    drop(db);
    std::fs::write(a.folder().join("shared.txt"), b"a's version").unwrap();
    std::fs::write(b.folder().join("shared.txt"), b"b's version").unwrap();
    std::fs::write(a.folder().join("only-a.txt"), b"from a").unwrap();
    std::fs::write(b.folder().join("only-b.txt"), b"from b").unwrap();
    let db = b.start();

    let ok = (0..100).any(|_| {
        let (ta, tb) = (tree(&a.folder()), tree(&b.folder()));
        if ta == tb && ta.len() == 4 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        false
    });
    if !ok {
        let names = |t: std::collections::BTreeMap<String, Option<Vec<u8>>>| {
            t.into_iter()
                .map(|(k, v)| {
                    format!(
                        "{k}={:?}",
                        v.map(|d| String::from_utf8_lossy(&d).into_owned())
                    )
                })
                .collect::<Vec<_>>()
        };
        panic!(
            "no convergence\na: {:?}\nb: {:?}\nsa: {:#?}\nsb: {:#?}",
            names(tree(&a.folder())),
            names(tree(&b.folder())),
            da.status().unwrap().folders,
            db.status().unwrap().folders
        );
    }
    let t = tree(&a.folder());
    let all: Vec<_> = t.values().flatten().cloned().collect();
    for want in [&b"a's version"[..], b"b's version", b"from a", b"from b"] {
        assert!(all.contains(&want.to_vec()), "missing {want:?} in {t:?}");
    }
    assert!(t.keys().any(|k| k.starts_with("shared.sync-conflict-")));
    let status = da.status().unwrap();
    assert_eq!(status.folders[0].conflicts.len(), 1);

    // Simultaneous live edits on both sides of several files.
    for i in 0..10 {
        std::fs::write(a.folder().join(format!("live-{i}.txt")), format!("a{i}")).unwrap();
        std::fs::write(b.folder().join(format!("live-{i}.txt")), format!("b{i}")).unwrap();
    }
    eventually(30, "convergence after live edits", || {
        let (ta, tb) = (tree(&a.folder()), tree(&b.folder()));
        ta == tb && da.status().unwrap().is_idle() && db.status().unwrap().is_idle()
    });
    // Nothing written last on either side may be lost.
    let all: Vec<_> = tree(&a.folder()).into_values().flatten().collect();
    for i in 0..10 {
        let (va, vb) = (format!("a{i}").into_bytes(), format!("b{i}").into_bytes());
        assert!(
            all.contains(&va) && all.contains(&vb),
            "lost an edit of live-{i}.txt"
        );
    }
}
