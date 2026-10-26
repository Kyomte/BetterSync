//! Finding the other machines on the user's tailnet via
//! `tailscale status --json`, so devices reachable over Tailscale need no
//! configured address. Which machine runs which BetterSync device is found
//! out by dialing: the pinned TLS handshake only succeeds with the right one.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Peer", default)]
    peer: std::collections::BTreeMap<String, Peer>,
}

#[derive(Debug, Deserialize)]
struct Peer {
    #[serde(rename = "TailscaleIPs", default)]
    ips: Vec<IpAddr>,
    #[serde(rename = "Online", default)]
    online: bool,
}

/// Addresses (`ip:port`) of the online peers in `tailscale status --json`
/// output. IPv4 first; empty unless Tailscale is running.
pub fn parse_status(json: &[u8], port: u16) -> Vec<SocketAddr> {
    let Ok(status) = serde_json::from_slice::<Status>(json) else {
        return Vec::new();
    };
    if status.backend_state != "Running" {
        return Vec::new();
    }
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for p in status.peer.values().filter(|p| p.online) {
        for ip in &p.ips {
            let a = SocketAddr::new(*ip, port);
            if ip.is_ipv4() { v4.push(a) } else { v6.push(a) }
        }
    }
    v4.sort();
    v6.sort();
    v4.extend(v6);
    v4
}

/// Where the `tailscale` CLI usually lives.
fn cli_candidates() -> Vec<PathBuf> {
    let mut c = vec![PathBuf::from("tailscale")];
    if cfg!(target_os = "macos") {
        c.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
    }
    if cfg!(windows) {
        c.push(r"C:\Program Files\Tailscale\tailscale.exe".into());
    }
    c
}

/// Ask the local Tailscale for its online peers. `None` if Tailscale is not
/// installed or not answering.
pub fn query(port: u16) -> Option<Vec<SocketAddr>> {
    for cli in cli_candidates() {
        let mut cmd = Command::new(&cli);
        cmd.args(["status", "--json"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            return Some(parse_status(&out.stdout, port));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "Version": "1.76.1",
      "BackendState": "Running",
      "Self": {"HostName": "laptop", "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Online": true},
      "Peer": {
        "nodekey:aa": {"HostName": "mac", "DNSName": "mac.tail1234.ts.net.", "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::2"], "Online": true},
        "nodekey:bb": {"HostName": "phone", "TailscaleIPs": ["100.64.0.9"], "Online": false},
        "nodekey:cc": {"HostName": "pi", "TailscaleIPs": ["100.64.0.4"], "Online": true, "ExitNode": false}
      }
    }"#;

    #[test]
    fn online_peers_ipv4_first() {
        let got = parse_status(SAMPLE.as_bytes(), 22027);
        let want: Vec<SocketAddr> = [
            "100.64.0.4:22027",
            "100.101.102.103:22027",
            "[fd7a:115c:a1e0::2]:22027",
        ]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn stopped_or_garbage_gives_nothing() {
        let stopped = SAMPLE.replace("\"Running\"", "\"Stopped\"");
        assert!(parse_status(stopped.as_bytes(), 1).is_empty());
        assert!(parse_status(b"not json", 1).is_empty());
    }
}
