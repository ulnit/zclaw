#!/usr/bin/env bash
# Build libzclaw as an iOS static library (arm64 + simulator).
# Run this ON A MAC — iOS targets can only be built with Apple's SDK.
# Requires: rustup + cargo, targets: aarch64-apple-ios, aarch64-apple-ios-sim, x86_64-apple-ios
#
# Usage:
#   cargo install cargo-lipo      # optional, for universal sim libs
#   ./build-ios.sh
set -euo pipefail
cd "$(dirname "$0")/../rust/zclaw"

OUT_DIR="$(pwd)/../../dist/ios"
mkdir -p "$OUT_DIR"

rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios 2>/dev/null || true

echo "==> device (arm64)"
cargo build --release --target aarch64-apple-ios
echo "==> simulator (arm64)"
cargo build --release --target aarch64-apple-ios-sim
echo "==> simulator (x86_64)"
cargo build --release --target x86_64-apple-ios

cp target/aarch64-apple-ios/release/libzclaw.a "$OUT_DIR/libzclaw-device.a"
# universal simulator lib
lipo -create \
  target/aarch64-apple-ios-sim/release/libzclaw.a \
  target/x86_64-apple-ios/release/libzclaw.a \
  -output "$OUT_DIR/libzclaw-sim.a" 2>/dev/null || cp target/aarch64-apple-ios-sim/release/libzclaw.a "$OUT_DIR/libzclaw-sim.a"

echo
echo "Done:"
ls -la "$OUT_DIR"
echo "Integrate in Xcode: add both .a files + header zclaw.h; see ios/Sources/ZClawBridge.h"
