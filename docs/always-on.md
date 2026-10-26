# Always-on peer (home server, Raspberry Pi)

An always-on peer is a normal BetterSync device that never sleeps, so every
folder has a copy that is reachable even when your laptop and desktop are
never online at the same time. It runs only the daemon (`bsyncd`) and the
CLI; no tray app.

## Install

Build for the machine (or let CI build `aarch64-unknown-linux-gnu` for a
64-bit Raspberry Pi OS), then follow the comments in
`contrib/systemd/bsyncd.service`.

## Configure

```sh
sudo -u bettersync bsync --config-dir /var/lib/bettersync/config init --name homeserver --listen 0.0.0.0:22027
```

Then edit `/var/lib/bettersync/config/config.json`:

```json
"always_on": {
  "auto_accept_folders": true,
  "folders_root": "/srv/bettersync"
},
"versions": { "max_days": 365, "max_gb": 200 }
```

- `auto_accept_folders`: when a paired device shares a folder with this
  machine, it is created as `<folders_root>/<folder id>` and starts syncing.
  Only paired devices can offer folders.
- `versions`: how long replaced and deleted content is kept in each
  folder's `.bsync/versions/` (a server is a good place to keep more).

Pair it with each of your devices (on both sides), and share folders with
it from those devices as usual:

```sh
bsync pair <SERVER-ID> --addr homeserver.local:22027   # or its Tailscale name
bsync folder share docs <SERVER-ID>
```

With Tailscale on the server, your devices find it from anywhere without a
configured address.
