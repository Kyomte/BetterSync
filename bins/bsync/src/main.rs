//! BetterSync command-line client.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bsync_core::DeviceId;
use bsyncd::Config;
use clap::{Parser, Subcommand};

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
    },
    /// Print this device's ID.
    Id,
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

fn parse_device(s: &str) -> Result<DeviceId> {
    s.parse()
        .with_context(|| format!("{s:?} is not a device ID"))
}

fn identity_id(dir: &std::path::Path) -> Result<DeviceId> {
    bsyncd::config::device_id(dir)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = cli.config_dir.unwrap_or_else(bsyncd::default_config_dir);
    match cli.command {
        Command::Init { name, listen } => {
            let mut c = Config::load_or_default(&dir)?;
            if let Some(n) = name {
                c.device_name = n;
            }
            if let Some(l) = listen {
                c.listen = l;
            }
            c.save(&dir)?;
            println!("{}", identity_id(&dir)?);
        }
        Command::Id => println!("{}", identity_id(&dir)?),
        Command::Pair {
            device,
            name,
            addresses,
        } => {
            let id = parse_device(&device)?;
            let mut c = Config::load_or_default(&dir)?;
            c.pair(id, name, addresses);
            c.save(&dir)?;
            println!("paired with {id}");
        }
        Command::Unpair { device } => {
            let id = parse_device(&device)?;
            let mut c = Config::load(&dir)?;
            c.unpair(&id);
            c.save(&dir)?;
            println!("unpaired {id}");
        }
        Command::Folder(FolderCommand::Add { id, path, share }) => {
            let devices = share
                .iter()
                .map(|s| parse_device(s))
                .collect::<Result<Vec<_>>>()?;
            let path = std::path::absolute(&path)?;
            let mut c = Config::load_or_default(&dir)?;
            c.add_folder(&id, path, &devices)?;
            c.save(&dir)?;
            println!("added folder {id}");
        }
        Command::Folder(FolderCommand::Share { id, device }) => {
            let mut c = Config::load(&dir)?;
            c.share_folder(&id, parse_device(&device)?)?;
            c.save(&dir)?;
            println!("shared {id} with {device}");
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
    }
    Ok(())
}
