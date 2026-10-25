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
