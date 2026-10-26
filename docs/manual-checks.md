# Manual checks (💻)

Items from `PLAN.md` that need a real Windows or macOS machine. Record each run
with date, OS version and result. CI only covers compile + automated tests.

| Milestone | Check | Windows | macOS |
|---|---|---|---|
| M2 | Editor "safe saves" (VS Code, Word) end up as a single edit in the index (`bsync status` shows no stray temp files; history continues) | not yet run | not yet run |
| M7 | PC ↔ Mac on the same Wi-Fi find each other via mDNS (no addresses configured) and sync | not yet run | not yet run |
| M7 | PC ↔ Mac over Tailscale with one device on a phone hotspot (static `100.x.y.z:22027` address) | not yet run | not yet run |
| M7 | Back on the same Wi-Fi, the connection moves to the LAN address within ~1 min | not yet run | not yet run |
| M8 | Windows ↔ macOS: `Readme.md` vs `README.md` created on each side is refused and shown as blocked on both | not yet run | not yet run |
| M8 | Windows ↔ macOS: a file named `café.txt` created on macOS (NFD) syncs as one file and edits flow both ways | not yet run | not yet run |
| M8 | Windows: a file open in Word is retried after closing it (shown as blocked meanwhile) | not yet run | n/a |
| M10 | Tray icon appears; colour follows state (green idle, blue syncing, grey paused, red needs attention, dark grey daemon not running) | not yet run | not yet run |
| M10 | Menu: summary line, Open folder ▸ each folder, Pause/Resume syncing, Status and devices… opens the status page, Quit | not yet run | not yet run |
| M10 | Tray starts `bsyncd` if it is not running | not yet run | not yet run |
| M10 | "Start at login" toggles the HKCU Run value / LaunchAgent plist and survives a log-out/in (Windows: launch the tray via `explorer.exe` when testing from a sandboxed shell) | not yet run | not yet run |
| M10 | Status page: pair a device, add a folder, pause/resume, conflict list | not yet run | not yet run |
| M10 | Idle CPU ≈ 0 and memory of `bsyncd` + `bsync-tray` (record below) | not yet run | not yet run |
| M11 | On a phone hotspot (Windows: metered connection; macOS: iPhone hotspot or Low Data Mode) `bsync status` shows "Network: metered" and a >50 MB file waits; back on Wi-Fi it syncs | not yet run | not yet run |
| M11 | `bsync bandwidth --up 500` limits upload to ~500 kB/s (watch Task Manager / Activity Monitor) | not yet run | not yet run |
| S5 | With Tailscale running on both and no `--addr` configured, PC ↔ Mac connect over the tailnet from different networks | not yet run | not yet run |
| S4 | Raspberry Pi 4/5 (64-bit OS): systemd service runs, auto-accepts a folder shared from the PC, keeps syncing across reboots | n/a (Pi) | n/a (Pi) |
| S3 | Android: install the CI debug APK, pair with the PC via the status page, sync a folder under the app's files directory; keeps syncing with the screen off (notification visible) | n/a (Android) | n/a (Android) |
| S1 | `"explorer_badges": true` in config.json, restart: synced files show a green tick, a file being edited/synced shows the arrows, the folder shows arrows while a conflict copy exists | not yet run | n/a |
| S1 | With badges on: editing in Word/VS Code, "Free up space" is refused or does nothing (file stays on disk), removing the folder from config removes the registration | not yet run | n/a |
| S1 | `bsync path-status <file>` prints synced / syncing / conflict / blocked | not yet run | not yet run |
| S3 | iPhone: build from Xcode with your Apple ID, pair with the PC over Wi-Fi (Bonjour, after allowing local network access), a folder shared from the PC appears in Files › On My iPhone › BetterSync; edits flow both ways while the app is open and finish within ~25 s after leaving it | n/a (iOS) | n/a (iOS) |
