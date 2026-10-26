# BetterSync — Plan

**iCloud Drive without the cloud.** Pick a folder on each of your devices; BetterSync keeps them identical by syncing peer-to-peer — directly over the LAN when devices share a network, over Tailscale when they don't. No central server holds your files. Every paired device keeps a full copy.

- Background engine (daemon) does the work; a small tray / menu-bar app controls it.
- Written in Rust. One codebase for Windows and macOS (Linux works too and is where CI and cloud sessions run).

---

## Goals

1. Small edits appear on the other device in **~1–2 s** on the same network.
2. **Never lose data.** No in-place overwrites, every replaced file goes to a versions folder, interrupted transfers resume.
3. Only changed parts of a file move over the network (content-defined chunking).
4. Concurrent edits are detected correctly (version vectors) and never silently dropped — the loser becomes a conflict copy.
5. Only paired devices can connect; all traffic is encrypted.
6. Idle cost near zero (no polling loops; OS file notifications only).

## Non-goals (v1)

- Sharing with other people / multi-user permissions.
- A web UI reachable from outside the user's devices.
- Mobile clients (stretch; see below).
- Being a backup tool (versions folder is a safety net, not backup).

## Open decisions

| Decision | Current default | Notes |
|---|---|---|
| v1 platforms | **Windows + macOS desktop** | Phones are a stretch milestone. Revisit after M8. |
| Transport | QUIC (`quinn`) with pinned self-signed certs | Alternative: TCP + `tokio-rustls`. QUIC gives multiplexed streams + better behaviour on lossy mobile links. |
| Conflict policy | Newest-by-version-vector wins, otherwise keep both (`name.sync-conflict-<date>-<device>.ext`) | CRDT text merge is a stretch goal. |
| Name | BetterSync | Rename before M0 if wanted. |

---

## Architecture

```
┌──────────────────────── device ────────────────────────┐
│                                                        │
│  tray app ──(localhost HTTP + token)──► daemon         │
│                                          │             │
│        ┌──────────── engine ─────────────┤             │
│        │ watcher → scanner → index (SQLite)            │
│        │ chunker (FastCDC + BLAKE3)                    │
│        │ sync planner (version vectors)                │
│        │ puller / applier (temp → verify → rename)     │
│        └──────────────┬────────────────────────────────┘
│                       │ protocol messages              │
│                 transport (QUIC, mTLS w/ pinned certs) │
│                       │                                │
│          discovery: mDNS (LAN) + static addrs (Tailscale)
└────────────────────────────────────────────────────────┘
```

### Crate layout (Cargo workspace)

| Crate | Responsibility |
|---|---|
| `crates/core` | Pure logic, no I/O: file metadata model, version vectors, chunk lists, sync planner (given local + remote index → actions). Heavily unit/property tested. |
| `crates/index` | SQLite index (`rusqlite`): files, chunks, versions, peers' last-seen sequence numbers. |
| `crates/fs` | Platform file-system layer: scanning, `notify` watcher + debouncer, atomic write, path normalisation (Unicode NFC, case folding, Windows reserved names), versions folder. |
| `crates/proto` | Wire messages (`serde` + `postcard`), framing, protocol version. |
| `crates/net` | QUIC transport, device identity, pairing, discovery. |
| `crates/engine` | Glues the above: per-folder state machine, pull scheduler, conflict handling. Runnable in-process with injected FS + transport (for the simulator). |
| `crates/sim` | Deterministic multi-peer simulator (in-memory FS + in-memory network, seeded RNG). |
| `bins/bsyncd` | Daemon + local control API. |
| `bins/bsync` | CLI (`bsync status`, `bsync pair`, `bsync folder add`, …). |
| `bins/bsync-tray` | Tray / menu-bar app (`tray-icon` + `tao`). |

### Core concepts

- **Device ID** — Ed25519 public key (base32, shown grouped like `ABCD-EFGH-…`). The TLS cert is self-signed from this key; peers pin each other's ID at pairing time.
- **Folder** — has a random folder ID, shared by all devices that sync it. Each device maps it to a local path.
- **File record** — `{path, size, mtime, permissions/flags, deleted, version_vector, chunk_list}`. Directories and symlinks are records too.
- **Chunks** — content-defined chunking with FastCDC (avg ~64 KiB, min 16 KiB, max 256 KiB), each identified by its BLAKE3 hash. An edit in the middle of a big file changes only the nearby chunks.
- **Version vector** — map `device_id → counter`. A local change increments this device's counter. Comparing two vectors gives *equal / newer / older / concurrent*. **Clocks are never trusted for ordering;** mtime is only a hint for "did this file change since the last scan".
- **Index exchange** — each device keeps a monotonically increasing local sequence number per folder. Peers request "everything since seq N", so reconnects only send deltas.
- **Pull** — for each file that is newer remotely: compute missing chunks, fetch them from any peer that has them (reuse chunks found locally in other files), write to a temp file in the same directory, verify the full-file hash, move the old version to `.bsync/versions/`, atomic rename into place.

### Invariants (must always hold)

1. Never write a user's file in place. Always temp file → fsync → hash check → rename.
2. Before replacing or deleting a file, the old content goes to `.bsync/versions/` (bounded by age/size settings).
3. A file changed locally since the last scan is never overwritten by a remote change without being re-evaluated (rescan right before rename; if it changed, treat as concurrent).
4. Concurrent changes never disappear: one wins, the other becomes a conflict copy that syncs like any other file.
5. Wall-clock time never decides ordering.
6. Only paired device IDs are accepted, on every connection.

### The nasty cases to design for

- Delete vs. edit on two devices → the edit wins (resurrects the file); never lose an edit to a delete.
- Delete a directory while another device adds a file inside it → directory is recreated with only the new file.
- Rename = delete + create; detect by matching chunk hashes so the content isn't re-sent.
- Case-only renames and case collisions (`File.txt` vs `file.txt`) between case-sensitive and case-insensitive file systems → refuse to create the collision locally, report it.
- Unicode normalisation (macOS NFD vs Windows NFC).
- Windows: reserved names (`CON`, `NUL`, …), trailing dots/spaces, max path, files locked by another process (retry with backoff, report).
- Editors that save by delete+recreate or write in bursts → debounce (default 1 s, configurable).
- Device asleep mid-transfer → partially fetched chunks persist; resume on reconnect.

### Discovery & connectivity

- **LAN:** announce/browse via mDNS (`mdns-sd`) with the device ID in a TXT record.
- **Tailscale:** user adds static addresses (`100.x.y.z` or MagicDNS name) per device. No Tailscale-specific code required — it is just another IP. Optional later: query `tailscale status --json` to auto-fill.
- If several addresses work, prefer the LAN one (lowest RTT).

### Bandwidth policies (later milestone)

- "Large files only on unmetered networks" (threshold configurable, default 50 MB). Uses Windows `NetworkCostType` / macOS `NWPath.isExpensive`.
- Upload/download rate limits.

---

## Milestones

Each milestone should fit in **one session** and end with green tests. Do them in order. Each gets its own branch (`m<N>-<slug>`) and PR.

Legend: 🐧 = fully verifiable on Linux / in a cloud session. 💻 = needs a manual check on the user's Windows/Mac machines (CI covers compile + unit tests only).

### M0 — Workspace, CI, simulator skeleton 🐧 ✅
- Cargo workspace with the crate layout above (empty crates compile).
- `rust-toolchain.toml` (stable), `rustfmt.toml`, clippy config.
- GitHub Actions matrix: `ubuntu-latest`, `windows-latest`, `macos-latest` running `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.
- `crates/sim`: in-memory FS trait + in-memory network trait, seeded RNG, one trivial test.

**Done when:** CI is green on all three OSes; `cargo test --workspace` passes locally.

### M1 — Scanner, chunker, index 🐧 ✅
- `fs`: recursive scan producing file records (skip `.bsync/`).
- `core`: FastCDC chunking (`fastcdc` crate) + BLAKE3 (`blake3` crate).
- `index`: SQLite schema + migrations; store/update records and chunk lists; local sequence numbers.
- Rescan only re-hashes files whose size/mtime changed.

**Done when:** tests cover: scan of a fixture tree; editing the middle of a 10 MB file changes only a few chunks; rescan without changes does no hashing.

### M2 — Watcher + debouncing 🐧/💻 ✅
- `notify` + `notify-debouncer-full`; events trigger targeted rescans of affected paths.
- Periodic full rescan fallback (default 1 h) for missed events.

**Done when:** integration test: create/modify/rename/delete in a temp dir updates the index within debounce + 500 ms. 💻 check on Windows & macOS that editor "safe saves" (VS Code, Word) are handled.

### M3 — Local one-way sync (in-process) 🐧 ✅
- `core` sync planner: given local index + remote index → list of actions (fetch, delete, mkdir, conflict).
- `engine`: applies actions with atomic writes + versions folder, fetching chunks through a trait (in-process implementation reads from the source folder).

**Done when:** two temp dirs, A → B, reach byte-identical state across a scripted set of creates/edits/deletes/renames; versions folder holds replaced content.

### M4 — Identity, pairing, transport 🐧 ✅
- Ed25519 key generation + storage; self-signed cert via `rcgen`; QUIC via `quinn` with custom cert verifier that pins device IDs.
- `proto`: Hello (protocol version, device ID, folders), IndexUpdate(since_seq), RequestChunk, ChunkData, Ping.
- Pairing: `bsync pair <device-id>` on both sides (manual mutual add, like Syncthing). No unpaired connections accepted.

**Done when:** two daemons on localhost (different ports + config dirs) pair, exchange indexes, and one-way sync works over the network. Unpaired peer is rejected (test).

### M5 — Two-way sync, version vectors, conflicts 🐧 ✅
- Version vectors on every record; planner handles newer/older/concurrent.
- Conflict copies named `name.sync-conflict-YYYYMMDD-HHMMSS-<shortdevice>.ext`.
- Delete-vs-edit, directory-delete-vs-add, rename detection by chunk match.
- Rescan-before-rename guard (invariant 3).

**Done when:** tests for every case in "The nasty cases" that are OS-independent; two daemons converge under concurrent edits.

### M6 — Simulator / fuzzing 🐧 ✅
- `sim` runs N (3–5) engines on the in-memory FS + network with random operations, random partitions/reconnects, random restarts.
- After quiescence assert: all folders identical, no lost edits (every edit present as a file or conflict copy), invariants hold.
- `proptest` or a seeded loop; failing seeds are printed and saved as regression tests.

**Done when:** 10,000 random runs pass in CI's nightly job (a smaller count runs on every PR).

### M7 — Discovery 🐧/💻 ✅
- mDNS announce/browse; static address list per device; connection manager with backoff, multiple addresses, prefer lowest RTT.

**Done when:** two daemons on the same machine find each other via mDNS (🐧 where multicast works). 💻 PC ↔ Mac on Wi-Fi, then over Tailscale with one device on phone hotspot.

### M8 — Robustness & platform rules 🐧/💻 ✅
- Resumable pulls (persist fetched chunks), ignore file (`.bsyncignore`, gitignore syntax via `ignore` crate), path normalisation + case-collision detection, Windows reserved names, locked-file retry, versions folder cleanup policy.

**Done when:** tests for each rule; 💻 Windows ↔ macOS case/Unicode collision manual check.

### M9 — Daemon, CLI, control API 🐧 ✅
- `bsyncd`: runs folders, exposes localhost HTTP JSON API (random token in config dir) — status, folders, peers, conflicts, pause/resume.
- `bsync` CLI over that API.
- Structured logging (`tracing`) to a rolling file.

**Done when:** `bsync status` shows folders/peers/progress; e2e test drives two daemons purely through the CLI.

### M10 — Tray / menu-bar app + autostart 💻 ✅ (code done; 💻 checklist pending)
- `bsync-tray`: icon states (idle, syncing, error, paused), menu: open folder, pause, status window, quit.
- Autostart: Windows HKCU Run value; macOS LaunchAgent plist. (Windows note: launch via `explorer.exe` when testing from a sandboxed shell, or the Run-key write may be virtualised.)
- Status window with pairing flow and conflict list (small webview or native — decide in the milestone).
  - **Decided:** a local web page served by the daemon at its control-API address and opened in the default browser (token in the URL fragment). No webview dependency, same page on every OS, and the API it uses is the one the CLI uses.

**Done when:** 💻 manual checklist passes on both OSes; idle CPU ≈ 0 and memory recorded in README.

### M11 — Bandwidth policies 💻 ✅ (code done; 💻 checks pending)
- Metered-network detection, big-files-on-Wi-Fi-only, rate limits.

### Stretch
- **S1 — Explorer / Finder integration:** status badges + on-demand files via Windows Cloud Files API (`cfapi`) and macOS File Provider extension. **Partly done; scope changed.**
  - ✅ Per-path sync state (`SyncEngine::path_state`: synced / syncing / conflict / blocked / ignored, summed up for folders), served by the daemon as `POST /api/path-status` and printed by `bsync path-status`. This is what any shell extension asks.
  - ✅ Windows Explorer badges (opt-in `explorer_badges`, see docs/explorer.md): each synced folder is registered as a Cloud Files sync root and its files are converted to *full* placeholders whose "in sync" flag follows `path_state`. Content stays on disk; the root's hydration policy (`ALWAYS_FULL`) makes Windows refuse dehydration and files are pinned. Opt-in because it was only verified on CI runners, not on a desktop.
  - ❌ **On-demand (online-only) files: decided against.** A dehydrated file has no local content, which breaks invariants 1–3: there is nothing to put in `.bsync/versions/` before replacing it, no bytes to rescan before a replace, and no merge base. Reading it would also need a peer online at that moment. Revisit only with a design for those.
  - ❌ macOS Finder badges not built: a Finder Sync or File Provider extension needs an Xcode app bundle signed with an Apple Developer ID, which cannot be built or tested from here. It would call `/api/path-status`.
- **S2 — CRDT merge for text files** (e.g. `automerge` / `yrs`) instead of conflict copies. ✅ **Done as a three-way merge instead of a CRDT.** A CRDT needs its own document (edit history) stored and synced beside every text file, and ordinary editors write plain files, so every save would have to be diffed into the CRDT anyway. Instead each record carries the hashes of its last 8 ancestors (`ancestors`, protocol 2), devices keep recent small text versions in `.bsync/bases/`, and the device holding the losing side of a conflict merges line by line from the closest common ancestor (`diffy`). Clean merge → one merged version that has seen both (the local bytes still go to `.bsync/versions/`); overlapping lines, binary files, files over 1 MiB or no known ancestor → conflict copy as before. The simulator has a text mode that checks no line a user wrote is ever lost.
- **S3 — Mobile client** (Android first; iOS background limits are severe). ✅ **Android.** The daemon runs inside the app: `crates/ffi` exposes it as a C ABI (`bsync_start`/`bsync_status_json`/`bsync_stop`) plus JNI entry points, built with `cargo ndk` for arm64-v8a and x86_64. The app (`android/`) keeps it alive in a `dataSync` foreground service (with an mDNS multicast lock) and shows the daemon's own status page in a WebView, so pairing and folders work exactly as on desktop. Folders must be real paths the app can write (its external files directory); Storage Access Framework folders are not supported. CI (`android.yml`) builds a debug APK. **iOS** (`ios/`, docs/ios.md): the same daemon as a static library (XCFramework) in a SwiftUI app showing the status page. iOS allows no long-running background work, so it syncs while open, for ~25 s after leaving, and in background app refresh / processing tasks iOS schedules — not always-on. Folders shared by paired devices are auto-accepted into the app's Documents (visible in the Files app). LAN discovery goes through the system's Bonjour (`NWBrowser` + an advertised `NetService`), because the daemon's own mDNS needs Apple's multicast entitlement; the app passes what it finds to the daemon. Changes are found by rescanning (every 15 s while running, and on returning to the foreground) instead of kqueue, which needs a file descriptor per file. CI builds it for devices and the simulator and syncs files both ways between the simulator app and a desktop daemon.
- **S4 — Always-on peer mode** for a home server / Raspberry Pi (headless daemon only). ✅ `always_on.auto_accept_folders` creates folders paired devices offer under `always_on.folders_root`; configurable versions retention; systemd unit; CI builds aarch64-linux. See docs/always-on.md.
- **S5 — Tailscale auto-discovery** via `tailscale status --json`. ✅ The daemon reads the online tailnet machines every 5 min and the dialer tries them (last) for paired devices it cannot otherwise reach; the pinned TLS handshake tells which machine is which device, and the match is remembered.

---

## Performance targets

| Scenario | Target |
|---|---|
| Small edit, same LAN | visible on peer ≤ 2 s (1 s debounce + transfer) |
| Hash throughput | ≥ 500 MB/s per core (BLAKE3) |
| 1 MB edit inside a 2 GB file | ≤ ~2 MB transferred |
| Idle daemon | ~0% CPU, < 30 MB RAM |
| Initial scan, 100k files | < 60 s on SSD |

## Testing strategy

1. **Unit + property tests** in `core` (version vectors, planner) — no I/O.
2. **Integration tests** with real temp dirs (`tempfile`) and two in-process engines.
3. **E2E tests** with two real daemon processes over localhost QUIC.
4. **Simulator** (M6) — the main confidence source for convergence.
5. **CI matrix** on Linux/Windows/macOS for platform-specific compile + FS behaviour.
6. **Manual checklists** (💻 items) recorded in `docs/manual-checks.md` with date + result.
