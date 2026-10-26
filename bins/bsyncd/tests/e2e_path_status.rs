//! S1: the daemon answers badge-state queries for absolute paths.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Dev, eventually};

#[test]
fn daemon_reports_path_states() {
    let a = Dev::new("a");
    std::fs::create_dir_all(a.folder().join("sub")).unwrap();
    std::fs::write(a.folder().join("sub/x.txt"), b"x").unwrap();
    a.edit(|cfg| {
        cfg.add_folder("docs", a.folder(), &[]).unwrap();
    });
    let d = a.start();
    let file = a.folder().join("sub/x.txt");
    let outside = std::env::temp_dir().join("not-synced.txt");
    eventually(20, "file is synced", || {
        d.path_status(vec![file.clone()]).unwrap()[0].state == "synced"
    });
    let s = d
        .path_status(vec![a.folder(), a.folder().join("sub"), outside.clone()])
        .unwrap();
    assert_eq!(s[0].folder.as_deref(), Some("docs"));
    assert_eq!(s[0].state, "synced");
    assert_eq!(s[1].state, "synced");
    assert_eq!(s[2].folder, None);
    assert_eq!(s[2].state, "unknown");
    drop(d);
}
