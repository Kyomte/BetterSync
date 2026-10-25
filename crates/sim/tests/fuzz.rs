//! M6: randomized runs. `BSYNC_FUZZ_RUNS` (default 200) seeds starting at
//! `BSYNC_FUZZ_SEED` (default 0) run on every `cargo test`; the nightly CI
//! job runs 10,000. A failing seed is printed — add it to `REGRESSIONS`.

use bsync_sim::fuzz::{FuzzConfig, run};

/// Seeds that once failed. They run on every test.
const REGRESSIONS: &[u64] = &[
    6,   // a chunk request answered "don't have it" was never retried
    3,   // a locally obsolete version contended with the winner forever
    136, // a device reused its own counter, so a new edit looked obsolete
    46,  // a merged vector was announced without saving the loser
    369, // a stale planned action replaced a just-saved conflict copy
    936, // a local delete that made us a conflict's loser was not re-planned
];

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn regression_seeds() {
    for &seed in REGRESSIONS {
        if let Err(e) = run(seed, FuzzConfig::default()) {
            panic!("{e}");
        }
    }
}

#[test]
fn random_runs() {
    let runs = env("BSYNC_FUZZ_RUNS", 200);
    let start = env("BSYNC_FUZZ_SEED", 0);
    let mut failures = Vec::new();
    for seed in start..start + runs {
        if let Err(e) = run(seed, FuzzConfig::default()) {
            eprintln!("{e}\n");
            failures.push(seed);
            if failures.len() >= 5 {
                break;
            }
        }
    }
    assert!(failures.is_empty(), "failing seeds: {failures:?}");
}
