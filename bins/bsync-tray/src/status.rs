//! Talking to the daemon's control API, and what the tray shows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The parts of `GET /api/status` the tray needs (unknown fields ignored).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Status {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub blocked: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Peer {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub connected: bool,
}

/// What the icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Syncing,
    Paused,
    Error,
    /// The daemon is not running / not answering.
    Offline,
}

impl Status {
    pub fn tray_state(&self) -> TrayState {
        let states: Vec<&str> = self.folders.iter().map(|f| f.state.as_str()).collect();
        if states.contains(&"error") || self.folders.iter().any(|f| !f.blocked.is_empty()) {
            TrayState::Error
        } else if states.contains(&"syncing") {
            TrayState::Syncing
        } else if !states.is_empty() && states.iter().all(|s| *s == "paused") {
            TrayState::Paused
        } else {
            TrayState::Idle
        }
    }

    pub fn all_paused(&self) -> bool {
        !self.folders.is_empty() && self.folders.iter().all(|f| f.state == "paused")
    }

    /// One line for the tooltip / first menu entry.
    pub fn summary(&self) -> String {
        let connected = self.peers.iter().filter(|p| p.connected).count();
        let conflicts: usize = self.folders.iter().map(|f| f.conflicts.len()).sum();
        let state = match self.tray_state() {
            TrayState::Idle => "Up to date",
            TrayState::Syncing => "Syncing…",
            TrayState::Paused => "Paused",
            TrayState::Error => "Needs attention",
            TrayState::Offline => "Not running",
        };
        let mut s = format!(
            "{state} · {connected}/{} device(s) connected",
            self.peers.len()
        );
        if conflicts > 0 {
            s += &format!(" · {conflicts} conflict(s)");
        }
        s
    }
}

/// Where the daemon's API is and how to authenticate.
#[derive(Debug, Clone)]
pub struct Api {
    pub base: String,
    pub token: String,
    agent: ureq::Agent,
}

impl Api {
    pub fn locate(config_dir: &Path) -> Option<Self> {
        let addr = std::fs::read_to_string(config_dir.join("api.addr")).ok()?;
        let token = std::fs::read_to_string(config_dir.join("api.token")).ok()?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        Some(Self {
            base: format!("http://{}", addr.trim()),
            token: token.trim().to_owned(),
            agent,
        })
    }

    pub fn status(&self) -> Result<Status> {
        let mut resp = self
            .agent
            .get(format!("{}/api/status", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()?;
        if !resp.status().is_success() {
            bail!("daemon answered {}", resp.status());
        }
        Ok(resp.body_mut().read_json()?)
    }

    pub fn post(&self, path: &str) -> Result<()> {
        let resp = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(serde_json::json!({}))?;
        if !resp.status().is_success() {
            bail!("daemon answered {}", resp.status());
        }
        Ok(())
    }

    /// The status page, with the token in the fragment (never sent to the
    /// server, never logged).
    pub fn status_page_url(&self) -> String {
        format!("{}/#token={}", self.base, self.token)
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BetterSync")
}

/// Start `bsyncd` (next to this executable) in the background.
pub fn start_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("executable has no directory")?;
    let daemon = dir.join(format!("bsyncd{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = std::process::Command::new(&daemon);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .with_context(|| format!("starting {}", daemon.display()))?;
    Ok(())
}

/// Open a folder or URL with the system's default handler.
pub fn open(target: &str) -> Result<()> {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", target]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.spawn().with_context(|| format!("opening {target}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: &str) -> Status {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn tray_state_from_status() {
        let s = status(
            r#"{"device_id":"X","folders":[{"id":"a","path":"/a","state":"idle"},{"id":"b","path":"/b","state":"syncing"}],"peers":[{"name":"mac","connected":true},{"name":"pc","connected":false}],"extra":1}"#,
        );
        assert_eq!(s.tray_state(), TrayState::Syncing);
        assert_eq!(s.summary(), "Syncing… · 1/2 device(s) connected");
        let s = status(r#"{"folders":[{"id":"a","path":"/a","state":"paused"}]}"#);
        assert_eq!(s.tray_state(), TrayState::Paused);
        assert!(s.all_paused());
        let s = status(
            r#"{"folders":[{"id":"a","path":"/a","state":"idle","blocked":[{"path":"x","reason":"y"}],"conflicts":["c"]}]}"#,
        );
        assert_eq!(s.tray_state(), TrayState::Error);
        assert!(s.summary().ends_with("1 conflict(s)"));
        assert_eq!(Status::default().tray_state(), TrayState::Idle);
    }
}
