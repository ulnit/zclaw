#!/usr/bin/env bash
# Build libzclaw for Android via cargo-ndk.
# Requires: rustup + Android targets (aarch64-linux-android, armv7-linux-androideabi),
#           cargo-ndk, and ANDROID_NDK_HOME pointing at an NDK (r26+).
#
# Usage:
#   ./build-android.sh            # builds arm64-v8a + armeabi-v7a
#   ./build-android.sh arm64      # arm64-v8a only
set -euo pipefail

# 🔴 先算绝对路径，再 cd。原实现第10行先 `cd .../rust/zclaw`，随后用
# `$(dirname "$0")/..` 推导 REPO_ROOT —— 但 $0 是相对路径（scripts/build-android.sh），
# cd 之后它就指向不存在的 scripts/.. ，报 "cd: scripts/..: No such file or directory"，
# 而 `||` 回退分支同样失败，OUT_DIR 变成空/畸形，cargo ndk -o 写不进去 →
# dist/ 里仍是上一次的旧 .so（时间戳不变），看起来"构建成功"实则没产出。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -W 2>/dev/null || cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT/rust/zclaw"

NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
export ANDROID_NDK_HOME="$NDK"
# 注意：MSYS/git-bash 下把 /e/... 传给原生 cargo-ndk 会被解析成 E:\e\... 畸形路径，
# 所以 REPO_ROOT 用 `pwd -W` 取 Windows 风格路径（上面已处理）。
OUT_DIR="$REPO_ROOT/dist/android"
mkdir -p "$OUT_DIR/arm64-v8a" "$OUT_DIR/armeabi-v7a"

build_one() {
  local target="$1" abi="$2" api="${3:-24}"
  echo "==> building $target ($abi, api $api)"
  cargo ndk --platform "$api" -t "$abi" -o "$OUT_DIR/$abi" build --release
  # 🔴 cargo ndk 的 -o 在 MSYS 下可能只打印 "Copying libraries to …" 而不真正拷贝。
  # 必须核对时间戳与 needle，否则会把旧 .so 当新产物部署上去。
  local out="$OUT_DIR/$abi/libzclaw.so"
  local built="target/$target/release/libzclaw.so"
  ls -la "$out"
  if [ -f "$built" ] && [ "$built" -nt "$out" ]; then
    echo "  ⚠️ cargo ndk 未拷贝（$built 比 $out 新）→ 手动拷贝"
    cp -f "$built" "$out"
    ls -la "$out"
  fi
  if ! grep -qa "reasoning_details" "$out"; then
    echo "  ❌ needle 校验失败：$out 不含 reasoning_details（可能是旧库）" >&2
    return 1
  fi
  echo "  ✓ needle 校验通过（含 reasoning_details）"
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
