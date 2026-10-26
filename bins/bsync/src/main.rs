//! BetterSync command-line client.
//!
//! Talks to the running daemon through its local control API. A few setup
//! commands (`init`, `id`, and `pair`/`folder` while the daemon is not
//! running) edit the configuration directly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bsync_core::DeviceId;
use bsyncd::Config;
use bsyncd::api::{BandwidthRequest, DeviceRequest, FolderRequest, OkDto, PairRequest, StatusDto};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Parser)]
#[command(version, about = "BetterSync command-line client")]
struct Cli {
    /// Configuration directory (default: the platform's per-user config dir).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the configuration and this device's identity.
    Init {
        /// Name shown to other devices.
        #[arg(long)]
        name: Option<String>,
        /// UDP address to listen on, e.g. 0.0.0.0:22027.
        #[arg(long)]
        listen: Option<String>,
        /// Loopback address of the control API, e.g. 127.0.0.1:22028.
        #[arg(long)]
        api: Option<String>,
    },
    /// Print this device's ID.
    Id,
    /// Show folders, devices and sync progress.
    Status {
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Trust a device. Run it on both devices with each other's ID.
    Pair {
        device: String,
        #[arg(long)]
        name: Option<String>,
        /// Static address (host:port), e.g. a Tailscale IP. Repeatable.
        #[arg(long = "addr")]
        addresses: Vec<String>,
    },
    /// Stop trusting a device.
    Unpair { device: String },
    /// Manage synced folders.
    #[command(subcommand)]
    Folder(FolderCommand),
    /// Pause syncing a folder.
    Pause { folder: String },
    /// Resume syncing a folder.
    Resume { folder: String },
    /// Rescan a folder for changes now.
    Rescan { folder: String },
    /// List conflict copies.
    Conflicts,
    /// Show or change bandwidth settings.
    Bandwidth {
        /// Treat the network as metered: auto (ask the OS), yes or no.
        #[arg(long)]
        metered: Option<String>,
        /// On metered networks, files larger than this many MB wait (0 = no limit).
        #[arg(long)]
        large_file_mb: Option<u64>,
        /// Upload limit in kB/s (0 = unlimited).
        #[arg(long)]
        up: Option<u64>,
        /// Download limit in kB/s (0 = unlimited).
        #[arg(long)]
        down: Option<u64>,
    },
}

#[derive(Subcommand)]
enum FolderCommand {
    /// Add a folder. Use the same ID on every device that syncs it.
    Add {
        id: String,
        path: PathBuf,
        /// Device to share with. Repeatable.
        #[arg(long = "share")]
        share: Vec<String>,
    },
    /// Share an existing folder with another device.
    Share { id: String, device: String },
    /// List folders.
    List,
}

/// Connection to the daemon's control API.
struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    /// `None` if the daemon is not running (no address file, or nothing
    /// answers there).
    fn connect(dir: &Path) -> Option<Self> {
        let addr = std::fs::read_to_string(bsyncd::api::addr_path(dir)).ok()?;
        let token = std::fs::read_to_string(bsyncd::api::token_path(dir)).ok()?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        let client = Self {
            base: format!("http://{}", addr.trim()),
            token: token.trim().to_owned(),
            agent,
        };
        client.status().ok().map(|_| client)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()?;
        Self::read(resp)
    }

    fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let resp = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(body)?;
        Self::read(resp)
    }

    fn read<T: DeserializeOwned>(mut resp: ureq::http::Response<ureq::Body>) -> Result<T> {
        if !resp.status().is_success() {
            let status = resp.status();
            let msg = resp
                .body_mut()
                .read_json::<OkDto>()
                .map(|e| e.message)
                .unwrap_or_else(|_| status.to_string());
            bail!("{msg}");
        }
        Ok(resp.body_mut().read_json()?)
    }

    fn status(&self) -> Result<StatusDto> {
        self.get("/api/status")
    }

    fn act(&self, path: &str, body: &impl Serialize) -> Result<()> {
        let r: OkDto = self.post(path, body)?;
        println!("{}", r.message);
        Ok(())
    }
}

fn parse_device(s: &str) -> Result<DeviceId> {
    s.parse()
        .with_context(|| format!("{s:?} is not a device ID"))
}

fn daemon(dir: &Path) -> Result<Client> {
    Client::connect(dir).context("the BetterSync daemon (bsyncd) is not running")
}

/// `println!` into a buffer.
macro_rules! outln {
    ($out:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        let _ = writeln!($out, $($arg)*);
    }};
}

/// Write to stdout, quietly stopping if the reader went away (e.g.
/// `bsync status | head`).
fn emit(text: &str) {
    use std::io::Write as _;
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

fn print_status(s: &StatusDto) {
    let mut out = String::new();
    outln!(out, "This device: {} ({})", s.device_id, s.device_name);
    if let Some(l) = &s.listen {
        outln!(out, "Listening on {l}");
    }
    if s.metered {
        outln!(
            out,
            "Network: metered (large files wait for an unmetered network)"
        );
    }
    out.push('\n');
    if s.folders.is_empty() {
        outln!(
            out,
            "No folders. Add one with `bsync folder add <id> <path> --share <device>`."
        );
    }
    for f in &s.folders {
        let progress = match f.state.as_str() {
            "syncing" => format!(
                "syncing: {} item(s) pending, {} transfer(s)",
                f.pending, f.transfers
            ),
            "error" => format!("error: {}", f.error.clone().unwrap_or_default()),
            other => other.to_owned(),
        };
        outln!(out, "Folder {}  {}", f.id, f.path);
        outln!(
            out,
            "  {progress}; {} item(s), shared with {} device(s)",
            f.local_entries,
            f.shared_with.len()
        );
        for b in &f.blocked {
            outln!(out, "  ! {}: {}", b.path, b.reason);
        }
        if !f.conflicts.is_empty() {
            outln!(
                out,
                "  {} conflict copy(ies), see `bsync conflicts`",
                f.conflicts.len()
            );
        }
    }
    out.push('\n');
    if s.peers.is_empty() {
        outln!(
            out,
            "No paired devices. Pair with `bsync pair <device-id>` on both devices."
        );
    }
    for p in &s.peers {
        let name = if p.name.is_empty() {
            "(unnamed)"
        } else {
            &p.name
        };
        match (p.connected, &p.address) {
            (true, Some(a)) => outln!(out, "Device {name}  {}  connected via {a}", p.id),
            (true, None) => outln!(out, "Device {name}  {}  connected", p.id),
            (false, _) => outln!(out, "Device {name}  {}  not connected", p.id),
        }
    }
    emit(&out);
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = cli.config_dir.unwrap_or_else(bsyncd::default_config_dir);
    match cli.command {
        Command::Init { name, listen, api } => {
            let mut c = Config::load_or_default(&dir)?;
            if let Some(n) = name {
                c.device_name = n;
            }
            if let Some(l) = listen {
                c.listen = l;
            }
            if let Some(a) = api {
                c.api_listen = a;
            }
            c.save(&dir)?;
            println!("{}", bsyncd::config::device_id(&dir)?);
        }
        Command::Id => println!("{}", bsyncd::config::device_id(&dir)?),
        Command::Status { json } => {
            let s = daemon(&dir)?.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_status(&s);
            }
        }
        Command::Pair {
            device,
            name,
            addresses,
        } => {
            let id = parse_device(&device)?;
            match Client::connect(&dir) {
                Some(c) => c.act(
                    "/api/pair",
                    &PairRequest {
                        id: id.to_string(),
                        name,
                        addresses,
                    },
                )?,
                None => {
                    let mut c = Config::load_or_default(&dir)?;
                    c.pair(id, name, addresses);
                    c.save(&dir)?;
                    println!("paired with {id} (takes effect when bsyncd starts)");
                }
            }
        }
        Command::Unpair { device } => {
            let id = parse_device(&device)?;
            match Client::connect(&dir) {
                Some(c) => c.act("/api/unpair", &DeviceRequest { id: id.to_string() })?,
                None => {
                    let mut c = Config::load(&dir)?;
                    c.unpair(&id);
                    c.save(&dir)?;
                    println!("unpaired {id}");
                }
            }
        }
        Command::Folder(FolderCommand::Add { id, path, share }) => {
            let devices = share
                .iter()
                .map(|s| parse_device(s))
                .collect::<Result<Vec<_>>>()?;
            let path = std::path::absolute(&path)?;
            match Client::connect(&dir) {
                Some(c) => c.act(
                    "/api/folders",
                    &FolderRequest {
                        id,
                        path: path.display().to_string(),
                        devices: devices.iter().map(ToString::to_string).collect(),
                    },
                )?,
                None => {
                    let mut c = Config::load_or_default(&dir)?;
                    c.add_folder(&id, path, &devices)?;
                    c.save(&dir)?;
                    println!("added folder {id}");
                }
            }
        }
        Command::Folder(FolderCommand::Share { id, device }) => {
            let device = parse_device(&device)?;
            match Client::connect(&dir) {
                Some(c) => c.act(
                    &format!("/api/folders/{id}/share"),
                    &DeviceRequest {
                        id: device.to_string(),
                    },
                )?,
                None => {
                    let mut c = Config::load(&dir)?;
                    c.share_folder(&id, device)?;
                    c.save(&dir)?;
                    println!("shared {id} with {device}");
                }
            }
        }
        Command::Folder(FolderCommand::List) => {
            for f in Config::load(&dir)?.folders {
                println!(
                    "{}\t{}\t{} device(s)",
                    f.id,
                    f.path.display(),
                    f.devices.len()
                );
            }
        }
        Command::Pause { folder } => {
            daemon(&dir)?.act(&format!("/api/folders/{folder}/pause"), &())?;
        }
        Command::Resume { folder } => {
            daemon(&dir)?.act(&format!("/api/folders/{folder}/resume"), &())?;
        }
        Command::Rescan { folder } => {
            daemon(&dir)?.act(&format!("/api/folders/{folder}/rescan"), &())?;
        }
        Command::Bandwidth {
            metered,
            large_file_mb,
            up,
            down,
        } => {
            let req = BandwidthRequest {
                metered,
                large_file_mb,
                max_upload_kbps: up,
                max_download_kbps: down,
            };
            match Client::connect(&dir) {
                Some(c) => c.act("/api/bandwidth", &req)?,
                None => {
                    let mut c = Config::load_or_default(&dir)?;
                    let b = &mut c.bandwidth;
                    if let Some(m) = &req.metered {
                        b.metered = m.parse()?;
                    }
                    b.large_file_mb = req.large_file_mb.unwrap_or(b.large_file_mb);
                    b.max_upload_kbps = req.max_upload_kbps.unwrap_or(b.max_upload_kbps);
                    b.max_download_kbps = req.max_download_kbps.unwrap_or(b.max_download_kbps);
                    println!("{:?}", c.bandwidth);
                    c.save(&dir)?;
                }
            }
        }
        Command::Conflicts => {
            let s = daemon(&dir)?.status()?;
            let mut any = false;
            for f in &s.folders {
                for c in &f.conflicts {
                    any = true;
                    println!("{}\t{}", f.id, Path::new(&f.path).join(c).display());
                }
            }
            if !any {
                println!("No conflicts.");
            }
        }
    }
    Ok(())
}
