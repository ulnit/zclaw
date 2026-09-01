#!/usr/bin/env bash
# Build libzclaw (Rust) for mobile targets from the ulnclaw source.
# Source repo (main): https://gitee.com/ushaw/ulnclaw
#
# Prerequisites:
#   rustup, cargo-ndk (Android), cargo-lipo / cargo-xcframework (iOS),
#   OHOS NDK + aarch64-unknown-linux-ohos target (HarmonyOS)
#
# Usage:
#   ./build-android.sh [ULNCLAW_SRC_DIR]     # -> dist/android/<abi>/libzclaw.so
#   ./build-ios.sh     [ULNCLAW_SRC_DIR]     # -> dist/ios/ZClawKit.xcframework
#   ./build-harmony.sh [ULNCLAW_SRC_DIR]     # -> dist/harmony/<abi>/libzclaw.so

set -euo pipefail

SRC="${1:-$(cd "$(dirname "$0")/../rust/ulnclaw" 2>/dev/null && pwd)}"
OUT="$(cd "$(dirname "$0")/.." && pwd)/dist"

if [ ! -f "$SRC/Cargo.toml" ]; then
    echo "ulnclaw source not found at: $SRC"
    echo "Clone it:  git clone https://gitee.com/ushaw/ulnclaw.git rust/ulnclaw"
    exit 1
fi

platform="${PLATFORM:-android}"
cd "$SRC"

case "$platform" in
  android)
    # cargo-ndk builds against the NDK bionic libc.
    # Requires: cargo install cargo-ndk ; $ANDROID_NDK_HOME set.
    for abi in arm64-v8a armeabi-v7a; do
        cargo ndk -t "$abi" -o "$OUT/android/$abi" build --release
    done
    echo "Android .so files -> $OUT/android/<abi>/libzclaw.so"
    ;;
  ios)
    # Static universal libs, then bundle into an XCFramework.
    # Requires: cargo install cargo-lipo
    cargo lipo --release           # produces aarch64-apple-ios + simulator slices
    mkdir -p "$OUT/ios"
    # Wrap into XCFramework (adjust headers path as needed)
    xcodebuild -create-xcframework \
        -library target/universal/release/libzclaw.a \
        -headers ../include \
        -output "$OUT/ios/ZClawKit.xcframework"
    ;;
  harmony)
    # OHOS target with the OHOS NDK sysroot.
    # Requires: rustup target add aarch64-unknown-linux-ohos ; OHOS_NDK_HOME set.
    export CC_aarch64_unknown_linux_ohos="$OHOS_NDK_HOME/native/llvm/bin/clang"
    export AR_aarch64_unknown_linux_ohos="$OHOS_NDK_HOME/native/llvm/bin/llvm-ar"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$CC_aarch64_unknown_linux_ohos"
    cargo build --release --target aarch64-unknown-linux-ohos
    mkdir -p "$OUT/harmony/arm64-v8a"
    cp target/aarch64-unknown-linux-ohos/release/libzclaw.so "$OUT/harmony/arm64-v8a/"
    ;;
  *)
    echo "Set PLATFORM=android|ios|harmony"
    exit 1
    ;;
esac
