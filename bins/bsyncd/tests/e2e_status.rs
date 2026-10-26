//! A freshly started daemon with local files and no peers settles to idle.

#![allow(clippy::unwrap_used)]

mod common;

use common::Dev;

/// Regression: the engine thread did not tick until the first event, so
/// work found by the startup scan stayed "pending" (shown as syncing).
#[test]
fn fresh_daemon_settles_without_any_event() {
    let a = Dev::new("a");
    let other = bsync_core::DeviceId::from_bytes([9; 32]);
    a.edit(|c| {
        c.local_discovery = false;
        c.pair(other, None, vec![]);
        c.add_folder("f", a.folder(), &[other]).unwrap();
    });
    std::fs::write(a.folder().join("x.txt"), b"x").unwrap();
    let d = a.start();
    // No event reaches the daemon meanwhile; the very first status request
    // must already see the startup work done (a request itself would
    // trigger a tick, so only the first answer tells).
    std::thread::sleep(std::time::Duration::from_millis(500));
    let s = d.status().unwrap();
    assert!(s.is_idle(), "{:?}", s.folders[0].engine);
    assert_eq!(s.folders[0].local_entries, 1);
}
