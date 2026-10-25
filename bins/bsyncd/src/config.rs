//! The daemon's configuration directory:
//!
//! ```text
//! <config dir>/
//!   config.json     devices, folders, listen address
//!   identity.key    this device's Ed25519 key (PKCS#8)
//!   index/<id>.db   one SQLite index per folder
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bsync_core::{DeviceId, FolderId};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 22027;
pub const DEFAULT_API_PORT: u16 = 22028;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Static addresses (`host:port`), e.g. a Tailscale `100.x.y.z:22027`.
    #[serde(default)]
    pub addresses: Vec<String>,
}

impl DeviceConfig {
    pub fn device_id(&self) -> Result<DeviceId> {
        self.id
            .parse()
            .with_context(|| format!("bad device ID {:?} in config", self.id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderConfig {
    pub id: String,
    pub path: PathBuf,
    /// Device IDs this folder is shared with.
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub paused: bool,
}

impl FolderConfig {
    pub fn folder_id(&self) -> FolderId {
        FolderId(self.id.clone())
    }

    pub fn shared_with(&self) -> BTreeSet<DeviceId> {
        self.devices.iter().filter_map(|d| d.parse().ok()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub device_name: String,
    /// UDP address to listen on.
    pub listen: String,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
    #[serde(default)]
    pub folders: Vec<FolderConfig>,
    /// Announce and find devices on the local network via mDNS.
    #[serde(default = "yes")]
    pub local_discovery: bool,
    /// Loopback address of the local control API (port 0 picks one; the
    /// bound address is written to `api.addr`).
    #[serde(default = "default_api")]
    pub api_listen: String,
}

fn default_api() -> String {
    format!("127.0.0.1:{DEFAULT_API_PORT}")
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: hostname(),
            listen: format!("0.0.0.0:{DEFAULT_PORT}"),
            devices: Vec::new(),
            folders: Vec::new(),
            local_discovery: true,
            api_listen: default_api(),
        }
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "my device".into())
}

/// This device's ID, creating its identity key if needed.
pub fn device_id(dir: &Path) -> Result<DeviceId> {
    Ok(bsync_net::Identity::load_or_create(&Config::identity_path(dir))?.device_id())
}

/// The platform's per-user config location for BetterSync.
pub fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BetterSync")
}

impl Config {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn identity_path(dir: &Path) -> PathBuf {
        dir.join("identity.key")
    }

    pub fn index_path(dir: &Path, folder: &str) -> PathBuf {
        // Folder IDs are user-chosen; keep the file name safe.
        let safe: String = folder
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        dir.join("index").join(format!("{safe}.db"))
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {} (run `bsync init` first?)", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn load_or_default(dir: &Path) -> Result<Self> {
        if Self::path(dir).exists() {
            Self::load(dir)
        } else {
            Ok(Self::default())
        }
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path(dir))?;
        Ok(())
    }

    pub fn paired(&self) -> BTreeSet<DeviceId> {
        self.devices
            .iter()
            .filter_map(|d| d.device_id().ok())
            .collect()
    }

    /// Paired devices and their static addresses, for the dialer.
    pub fn dial_targets(&self) -> std::collections::BTreeMap<DeviceId, Vec<String>> {
        self.devices
            .iter()
            .filter_map(|d| Some((d.device_id().ok()?, d.addresses.clone())))
            .collect()
    }

    pub fn device(&self, id: &DeviceId) -> Option<&DeviceConfig> {
        self.devices
            .iter()
            .find(|d| d.device_id().ok().as_ref() == Some(id))
    }

    /// Add or update a paired device.
    pub fn pair(&mut self, id: DeviceId, name: Option<String>, addresses: Vec<String>) {
        let text = id.to_string();
        match self
            .devices
            .iter_mut()
            .find(|d| d.device_id().ok() == Some(id))
        {
            Some(d) => {
                if let Some(n) = name {
                    d.name = n;
                }
                for a in addresses {
                    if !d.addresses.contains(&a) {
                        d.addresses.push(a);
                    }
                }
            }
            None => self.devices.push(DeviceConfig {
                id: text,
                name: name.unwrap_or_default(),
                addresses,
            }),
        }
    }

    pub fn unpair(&mut self, id: &DeviceId) {
        self.devices
            .retain(|d| d.device_id().ok().as_ref() != Some(id));
        for f in &mut self.folders {
            f.devices
                .retain(|d| d.parse::<DeviceId>().ok().as_ref() != Some(id));
        }
    }

    pub fn add_folder(&mut self, id: &str, path: PathBuf, share_with: &[DeviceId]) -> Result<()> {
        if id.is_empty() {
            bail!("folder ID must not be empty");
        }
        if self.folders.iter().any(|f| f.id == id) {
            bail!("folder {id:?} already exists");
        }
        self.folders.push(FolderConfig {
            id: id.to_owned(),
            path,
            devices: share_with.iter().map(ToString::to_string).collect(),
            paused: false,
        });
        Ok(())
    }

    pub fn share_folder(&mut self, id: &str, device: DeviceId) -> Result<()> {
        let f = self
            .folders
            .iter_mut()
            .find(|f| f.id == id)
            .with_context(|| format!("no folder {id:?}"))?;
        let text = device.to_string();
        if !f.devices.contains(&text) {
            f.devices.push(text);
        }
        Ok(())
    }

    pub fn folder_mut(&mut self, id: &str) -> Option<&mut FolderConfig> {
        self.folders.iter_mut().find(|f| f.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_pairing_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        let d = DeviceId::from_bytes([5; 32]);
        c.pair(d, Some("mac".into()), vec!["100.64.0.2:22027".into()]);
        c.pair(d, None, vec!["192.168.1.9:22027".into()]);
        c.add_folder("docs", PathBuf::from("/x"), &[d]).unwrap();
        assert!(c.add_folder("docs", PathBuf::from("/y"), &[]).is_err());
        c.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded, c);
        assert_eq!(loaded.devices[0].addresses.len(), 2);
        assert_eq!(loaded.folders[0].shared_with(), [d].into_iter().collect());

        c.unpair(&d);
        assert!(c.devices.is_empty());
        assert!(c.folders[0].devices.is_empty());
    }
}
