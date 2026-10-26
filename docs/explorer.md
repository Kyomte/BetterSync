# File manager integration (S1)

## Sync state of a path (all platforms)

```sh
bsync path-status ~/Sync/notes.txt ~/Sync/Photos
# synced    /home/me/Sync/notes.txt
# syncing   /home/me/Sync/Photos
```

States: `synced`, `syncing` (a local change not yet scanned, or a remote
change not yet applied), `conflict` (a conflict copy, or a folder with one
inside), `blocked` (with the reason, e.g. a locked file), `ignored`,
`unknown` (not in any synced folder), `paused`, `error`.

Programs can ask the daemon directly: `POST /api/path-status` with
`{"paths": ["/abs/path", ...]}` and the API token as a bearer token (see
`api.token` and `api.addr` in the config directory).

## Windows Explorer badges

Off by default. To turn it on, add `"explorer_badges": true` to
`config.json` and restart `bsyncd` (or the tray app).

Each synced folder is then registered with Windows as a cloud sync root
(the same mechanism OneDrive uses). Explorer shows:

- a green tick on files and folders that are in sync;
- the sync arrows on anything being synced, changed and not yet synced, a
  blocked file, and on folders that contain a conflict copy.

Files are never made "online-only": the content always stays on disk, and
Windows refuses "Free up space" for these folders. Removing a folder from
BetterSync removes its registration; the files stay as ordinary files.

Limitations:

- A synced folder inside another sync root (OneDrive, Dropbox) cannot be
  registered; badges stay off for it (see the daemon log).
- Turning the setting off needs a restart and leaves the registration in
  place until the folder is removed.

## macOS Finder

Not available yet. Finder badges need a Finder Sync or File Provider app
extension, signed with an Apple Developer ID. It would ask
`/api/path-status` for the state of each file.
