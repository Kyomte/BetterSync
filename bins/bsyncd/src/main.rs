//! BetterSync daemon.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

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
    std::fs::create_dir_all(&dir)?;

    // Structured logs: to stderr, and to a daily-rotated file (7 kept) in
    // <config dir>/logs. RUST_LOG overrides the default level.
    let file = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("bsyncd")
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir.join("logs"))
        .context("opening the log directory")?;
    let (file, _guard) = tracing_appender::non_blocking(file);
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter()),
        )
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(file)
                .with_filter(filter()),
        )
        .init();

    let daemon = bsyncd::Daemon::start(&dir)?;
    println!(
        "bsyncd running as {} on {} (control API on {})",
        daemon.device_id(),
        daemon.local_addr(),
        daemon.api_addr()
    );
    // Runs until killed.
    loop {
        std::thread::park();
    }
}
