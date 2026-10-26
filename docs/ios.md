# iPhone / iPad app

`ios/` is a SwiftUI app that runs the BetterSync daemon inside the app
(the same Rust code as on desktop, through `crates/ffi`) and shows the
daemon's status page.

## What to expect

iOS does not let apps keep running in the background, so unlike the
desktop and Android apps BetterSync on iOS is **not always on**:

- it syncs while the app is open;
- after you leave it, it keeps going for up to ~25 seconds to finish;
- iOS wakes it now and then (background app refresh, and longer runs
  typically while charging) to catch up, at times iOS chooses.

So a file you save on the PC shows up on the phone the next time you open
BetterSync (or after one of those background runs), not instantly.

## Using it

1. Pair: on the PC `bsync pair <phone ID>` (the ID is on the app's status
   page), and on the phone enter the PC's ID in the status page's pairing
   form. On the same Wi-Fi they find each other; otherwise add the PC's
   address (e.g. its Tailscale IP and port) when pairing.
2. Share a folder with the phone from the PC
   (`bsync folder share <folder> <phone ID>`). It appears automatically in
   **Files › On My iPhone › BetterSync › <folder>**, no setup on the phone.
3. Files edited in that folder (from Files or any app that opens files in
   place) sync back.

The first time, iOS asks to allow finding devices on the local network;
LAN discovery needs that permission.

## Building

On a Mac with Xcode 15+, Rust and [XcodeGen](https://github.com/yonaskolb/XcodeGen):

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
ios/build-rust.sh            # builds ios/BsyncFFI.xcframework
cd ios && xcodegen generate  # writes BetterSync.xcodeproj
open BetterSync.xcodeproj
```

Choose your team under Signing & Capabilities (a free Apple ID is enough
to run it on your own device for 7 days) and run.

CI (workflow "iOS") builds the app for devices (unsigned) and the
simulator, then runs it in the simulator, pairs it with a desktop daemon
and checks that files sync both ways.
