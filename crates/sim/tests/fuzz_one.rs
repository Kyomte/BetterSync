//! Run a single seed with full diagnostics: `BSYNC_SEED=6 cargo test -p
//! bsync-sim --test fuzz_one -- --nocapture`.

use bsync_sim::fuzz::{FuzzConfig, run};

#[test]
fn one_seed() {
    let Some(seed) = std::env::var("BSYNC_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    else {
        return;
    };
    // RUST_LOG=bsync_engine=debug shows every scan change and applied action.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .try_init();
    let config = FuzzConfig {
        text: std::env::var_os("BSYNC_FUZZ_TEXT").is_some(),
        ..FuzzConfig::default()
    };
    if let Err(e) = run(seed, config) {
        panic!("{e}");
    }
}
