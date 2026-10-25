//! BetterSync daemon.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "BetterSync daemon")]
struct Args {
    /// Configuration directory (default: the platform's per-user config dir).
    #[arg(long)]
    config_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let dir = args.config_dir.unwrap_or_else(bsyncd::default_config_dir);
    let daemon = bsyncd::Daemon::start(&dir)?;
    println!(
        "bsyncd running as {} on {}",
        daemon.device_id(),
        daemon.local_addr()
    );
    // Runs until killed.
    loop {
        std::thread::park();
    }
}
