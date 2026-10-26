//! M9: two daemon processes driven purely through the `bsync` CLI.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::{eventually, tree};

fn bin(name: &str) -> PathBuf {
    // Integration tests run from target/<profile>/deps; binaries live one up.
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Build `bsync` once per run, even if an older binary exists: running a
/// stale one tests old code.
fn ensure_built() {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let ok = Command::new(env!("CARGO"))
            .args(["build", "-p", "bsync"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "building bsync failed");
    });
}

fn cli(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(bin("bsync"))
        .arg("--config-dir")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bsync {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(dir: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_bsyncd"))
        .arg("--config-dir")
        .arg(dir)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait until the control API answers.
    eventually(20, "daemon API up", || {
        Command::new(bin("bsync"))
            .arg("--config-dir")
            .arg(dir)
            .arg("status")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    Daemon(child)
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn two_daemons_through_the_cli() {
    ensure_built();
    let tmp = tempfile::tempdir().unwrap();
    let (ca, cb) = (tmp.path().join("cfg-a"), tmp.path().join("cfg-b"));
    let (fa, fb) = (tmp.path().join("folder-a"), tmp.path().join("folder-b"));
    let (pa, pb) = (free_udp_port(), free_udp_port());

    let id_a = cli(
        &ca,
        &[
            "init",
            "--name",
            "alpha",
            "--listen",
            &format!("127.0.0.1:{pa}"),
            "--api",
            "127.0.0.1:0",
        ],
    );
    let id_b = cli(
        &cb,
        &[
            "init",
            "--name",
            "beta",
            "--listen",
            &format!("127.0.0.1:{pb}"),
            "--api",
            "127.0.0.1:0",
        ],
    );
    let (id_a, id_b) = (id_a.trim().to_owned(), id_b.trim().to_owned());
    assert_eq!(cli(&ca, &["id"]).trim(), id_a);

    let _da = start(&ca);
    let _db = start(&cb);

    // Everything below goes through the running daemons' API.
    cli(
        &ca,
        &[
            "pair",
            &id_b,
            "--name",
            "beta",
            "--addr",
            &format!("127.0.0.1:{pb}"),
        ],
    );
    cli(
        &cb,
        &[
            "pair",
            &id_a,
            "--name",
            "alpha",
            "--addr",
            &format!("127.0.0.1:{pa}"),
        ],
    );
    std::fs::create_dir_all(&fa).unwrap();
    std::fs::write(fa.join("hello.txt"), b"hello over the CLI").unwrap();
    cli(
        &ca,
        &[
            "folder",
            "add",
            "docs",
            fa.to_str().unwrap(),
            "--share",
            &id_b,
        ],
    );
    cli(
        &cb,
        &[
            "folder",
            "add",
            "docs",
            fb.to_str().unwrap(),
            "--share",
            &id_a,
        ],
    );

    eventually(30, "file synced", || {
        tree(&fb) == tree(&fa) && !tree(&fa).is_empty()
    });

    let status = cli(&ca, &["status"]);
    assert!(status.contains("Folder docs"), "{status}");
    assert!(
        status.contains("beta") && status.contains("connected"),
        "{status}"
    );
    eventually(20, "folder idle", || {
        cli(&ca, &["status"]).contains("idle;")
    });

    // Pause on b: a's edits stop arriving; resume: they arrive.
    cli(&cb, &["pause", "docs"]);
    assert!(cli(&cb, &["status"]).contains("paused"));
    std::fs::write(fa.join("while-paused.txt"), b"later").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!fb.join("while-paused.txt").exists());
    cli(&cb, &["resume", "docs"]);
    eventually(30, "sync after resume", || {
        fb.join("while-paused.txt").exists()
    });

    let json = cli(&cb, &["status", "--json"]);
    let parsed: bsyncd::api::StatusDto = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.device_id, id_b);
    assert_eq!(cli(&cb, &["conflicts"]).trim(), "No conflicts.");
    let synced = fb.join("while-paused.txt");
    eventually(20, "path-status says synced", || {
        cli(&cb, &["path-status", synced.to_str().unwrap()]).starts_with("synced\t")
    });

    // A wrong token is refused.
    let addr = std::fs::read_to_string(cb.join("api.addr")).unwrap();
    let resp = ureq_get(&format!("http://{}/api/status", addr.trim()));
    assert_eq!(resp, 401);
}

fn ureq_get(url: &str) -> u16 {
    // A bare HTTP request without the token.
    use std::io::{Read, Write};
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').unwrap();
    let mut s = std::net::TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn metered_network_holds_back_large_files() {
    ensure_built();
    let tmp = tempfile::tempdir().unwrap();
    let (ca, cb) = (tmp.path().join("cfg-a"), tmp.path().join("cfg-b"));
    let (fa, fb) = (tmp.path().join("folder-a"), tmp.path().join("folder-b"));
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let id_a = cli(
        &ca,
        &[
            "init",
            "--listen",
            &format!("127.0.0.1:{pa}"),
            "--api",
            "127.0.0.1:0",
        ],
    );
    let id_b = cli(
        &cb,
        &[
            "init",
            "--listen",
            &format!("127.0.0.1:{pb}"),
            "--api",
            "127.0.0.1:0",
        ],
    );
    let (id_a, id_b) = (id_a.trim().to_owned(), id_b.trim().to_owned());
    // b is on a "metered" network; files over 1 MB wait.
    cli(
        &cb,
        &["bandwidth", "--metered", "yes", "--large-file-mb", "1"],
    );
    let _da = start(&ca);
    let _db = start(&cb);
    assert!(cli(&cb, &["status"]).contains("Network: metered"));
    cli(&ca, &["pair", &id_b, "--addr", &format!("127.0.0.1:{pb}")]);
    cli(&cb, &["pair", &id_a, "--addr", &format!("127.0.0.1:{pa}")]);
    std::fs::create_dir_all(&fa).unwrap();
    std::fs::write(fa.join("small.txt"), b"small").unwrap();
    std::fs::write(fa.join("big.bin"), vec![3u8; 3 * 1024 * 1024]).unwrap();
    cli(
        &ca,
        &["folder", "add", "f", fa.to_str().unwrap(), "--share", &id_b],
    );
    cli(
        &cb,
        &["folder", "add", "f", fb.to_str().unwrap(), "--share", &id_a],
    );

    eventually(30, "small file synced", || fb.join("small.txt").exists());
    eventually(10, "big file reported as waiting", || {
        cli(&cb, &["status"]).contains("unmetered")
    });
    assert!(!fb.join("big.bin").exists());

    let out = cli(&cb, &["bandwidth", "--metered", "no", "--down", "0"]);
    assert!(out.contains("metered: No"), "{out}");
    eventually(30, "big file synced once unmetered", || {
        std::fs::metadata(fb.join("big.bin")).is_ok_and(|m| m.len() == 3 * 1024 * 1024)
    });
}
