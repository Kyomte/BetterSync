# BetterSync — instructions for Claude sessions

Peer-to-peer folder sync ("iCloud Drive without the cloud") in Rust. **Read `PLAN.md` first** — it is the source of truth for architecture, invariants and milestones.

## How work is organised

- The user starts a session with "do milestone N". Implement exactly that milestone's scope from `PLAN.md`, nothing from later milestones.
- Work on a branch `m<N>-<slug>` (e.g. `m1-scanner-index`) and open a PR against `main`. Never push to `main` directly.
- A milestone is done only when its "Done when" list passes. Say clearly which 💻 items you could not verify (cloud sessions run on Linux and cannot check Windows/macOS behaviour — CI covers compile + tests there).
- If a decision in `PLAN.md` turns out wrong, update `PLAN.md` in the same PR and explain why in the PR description.
- Tick the milestone off in `PLAN.md` (append ✅ to its heading) in the PR that completes it.

## Setup

- If `cargo` is missing: `curl https://sh.rustup.rs -sSf | sh -s -- -y && . "$HOME/.cargo/env"`.
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint (CI enforces): `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`

## Non-negotiable invariants

1. Never write a user's file in place: temp file in the same dir → fsync → verify hash → rename.
2. Old content goes to `.bsync/versions/` before any replace or delete.
3. Rescan a file right before replacing it; if it changed locally, treat as a concurrent change.
4. Concurrent changes never disappear — loser becomes a conflict copy.
5. Wall-clock time never decides ordering — version vectors do.
6. Only paired device IDs are accepted on any connection.

## Code conventions

- `core` stays pure (no I/O, no async) so it can be property-tested and used by the simulator.
- File system and network access go through traits so `crates/sim` can swap in in-memory versions.
- Errors: `thiserror` in libraries, `anyhow` in binaries. Logging: `tracing`.
- Every bug fix comes with a regression test; simulator failures are saved as seeded regression tests.

## Commits

- Commit author: `kyomeee <kio.mingote@gmail.com>`.
- End commit messages with:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
