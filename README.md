# BetterSync

iCloud Drive without the cloud. Pick a folder on each of your devices and BetterSync keeps them identical, peer-to-peer:

- **At home:** devices sync directly over your Wi-Fi/LAN — fast, and no internet used.
- **Away:** devices sync over [Tailscale](https://tailscale.com) — encrypted, works from mobile data.
- **No server:** every paired device keeps a full copy. Nothing is stored by a third party.
- **Only changes move:** files are split into content-defined chunks; editing 1 MB of a 2 GB file sends ~1 MB.
- **Never loses data:** atomic writes, a versions folder for replaced files, and conflict copies when two devices edit at once.

Status: **planning** — see [PLAN.md](PLAN.md) for the architecture and milestones.

Targets Windows and macOS (Linux works too). Written in Rust.
