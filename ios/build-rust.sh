#!/bin/sh
# Builds the daemon (crates/ffi) as a static library for iOS devices and the
# simulator and packages it as ios/BsyncFFI.xcframework. Needs macOS with
# Xcode and: rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
set -eu
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
export IPHONEOS_DEPLOYMENT_TARGET=16.0
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo rustc -p bsync-ffi --profile "$PROFILE" --target "$target" --crate-type staticlib
done

dir() { echo "target/$1/$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")"; }
out=target/ios
rm -rf "$out" ios/BsyncFFI.xcframework
mkdir -p "$out/sim" "$out/headers"
lipo -create \
    "$(dir aarch64-apple-ios-sim)/libbsync_ffi.a" \
    "$(dir x86_64-apple-ios)/libbsync_ffi.a" \
    -output "$out/sim/libbsync_ffi.a"
cp crates/ffi/include/bsync.h crates/ffi/include/module.modulemap "$out/headers/"
xcodebuild -create-xcframework \
    -library "$(dir aarch64-apple-ios)/libbsync_ffi.a" -headers "$out/headers" \
    -library "$out/sim/libbsync_ffi.a" -headers "$out/headers" \
    -output ios/BsyncFFI.xcframework
echo "built ios/BsyncFFI.xcframework"
