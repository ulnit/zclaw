#!/usr/bin/env bash
# Build libzclaw for Android via cargo-ndk.
# Requires: rustup + Android targets (aarch64-linux-android, armv7-linux-androideabi),
#           cargo-ndk, and ANDROID_NDK_HOME pointing at an NDK (r26+).
#
# Usage:
#   ./build-android.sh            # builds arm64-v8a + armeabi-v7a
#   ./build-android.sh arm64      # arm64-v8a only
set -euo pipefail
cd "$(dirname "$0")/../rust/zclaw"

NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
export ANDROID_NDK_HOME="$NDK"
OUT_DIR="$(pwd)/../../dist/android"
mkdir -p "$OUT_DIR/arm64-v8a" "$OUT_DIR/armeabi-v7a"

build_one() {
  local target="$1" abi="$2" api="${3:-24}"
  echo "==> building $target ($abi, api $api)"
  cargo ndk --platform "$api" -t "$abi" -o "$OUT_DIR/$abi" build --release
  ls -la "$OUT_DIR/$abi/libzclaw.so"
}

case "${1:-all}" in
  arm64)  build_one aarch64-linux-android arm64-v8a ;;
  armv7)  build_one armv7-linux-androideabi armeabi-v7a ;;
  all|*)  build_one aarch64-linux-android arm64-v8a
          build_one armv7-linux-androideabi armeabi-v7a ;;
esac

echo
echo "Done. Copy the .so files into the app: jniLibs/<abi>/libzclaw.so"
echo "(Android app also needs libzclaw_jni.so, built by the app's own CMake — see app/src/main/cpp)"
