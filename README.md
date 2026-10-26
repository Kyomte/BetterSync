# BetterSync

iCloud Drive without the cloud. Pick a folder on each of your devices and BetterSync keeps them identical, peer-to-peer:

- **At home:** devices sync directly over your Wi-Fi/LAN — fast, and no internet used.
- **Away:** devices sync over [Tailscale](https://tailscale.com) — encrypted, works from mobile data.
- **No server:** every paired device keeps a full copy. Nothing is stored by a third party.
- **Only changes move:** files are split into content-defined chunks; editing 1 MB of a 2 GB file sends ~1 MB.
- **Never loses data:** atomic writes, a versions folder for replaced files, and conflict copies when two devices edit at once.

Status: **M0–M11 implemented** — see [PLAN.md](PLAN.md) for the architecture and milestones, and [docs/manual-checks.md](docs/manual-checks.md) for what still needs checking on real Windows/Mac machines.

## Quick start

```sh
cargo build --release
# On each device:
bsync init --name laptop          # prints this device's ID
bsyncd &                          # or bsync-tray, which starts the daemon
# Pair (on both devices, with the other one's ID; --addr for Tailscale):
bsync pair <OTHER-DEVICE-ID> --name mac --addr 100.x.y.z:22027
# Share a folder (same folder ID on both devices):
bsync folder add docs ~/Documents/Shared --share <OTHER-DEVICE-ID>
bsync status
```

On mobile data (Windows metered connections, macOS hotspots or Low Data
Mode) files over 50 MB wait for an unmetered network; see
`bsync bandwidth --help` for that threshold and upload/download limits.

Devices on the same network find each other via mDNS, and devices on your
tailnet are found through `tailscale status` (when Tailscale is installed),
so `--addr` is only needed for other routed networks or non-default ports. Put gitignore-style
patterns in `.bsyncignore` at the folder root to keep files local.

## Resource use when idle

Measured on Linux (release build, one folder with 1,000 files, one paired
device offline, mDNS on), sampled over 60 s after start-up:

| Process | CPU | Memory (RSS) |
|---|---|---|
| `bsyncd` | 0.02 s per minute (≈0.03 %) | 16.7 MB |

Windows and macOS numbers (including `bsync-tray`) are still to be recorded
— see docs/manual-checks.md.

Targets Windows and macOS (Linux works too). Written in Rust.

## Android

`android/` is an Android app that runs the daemon in a foreground service
and shows the status page. CI builds a debug APK (workflow "Android",
artifact `bettersync-debug-apk`). To build locally you need the Android SDK
and NDK:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release -p bsync-ffi
gradle -p android assembleDebug
```

## iPhone / iPad

`ios/` is the iOS app (syncs while open and when iOS grants background
time; not always-on). See docs/ios.md for what to expect and how to build.
