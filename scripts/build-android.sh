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
# 🔴 必须用 `{ pwd -W || pwd; }` 花括号成组：裸写 `pwd -W 2>/dev/null || pwd`
#    再嵌进 `$(cd .. && A || B)` 时，shell 按 `((cd&&A)||(B))` 结合，
#    `pwd -W` 成功后整个 `&&` 链的右侧仍可能再执行一次 pwd，
#    命令替换捕获到**两行路径**（"E:/...\n/e/..."），cd 报 No such file。
#    花括号让「取 Windows 风格路径，失败退回 POSIX 路径」成为单一命令。
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && { pwd -W 2>/dev/null || pwd; })"

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
  # 🔴 needle 必须能**区分版本**。原先只校验 reasoning_details —— 那是 0.5.0 起
  #    就有的字符串，旧库同样通过，等于没校验（部署过旧库而不自知）。
  #    改校验当前版本串，并把「新 prompt 独有片段」一起验，确保 prompt 改动真进了库。
  local ver_needle="0.5.2-mobile"
  if ! grep -qa "$ver_needle" "$out"; then
    echo "  ❌ needle 校验失败：$out 不含 $ver_needle（是旧库！）" >&2
    return 1
  fi
  if ! grep -qa "reasoning_details" "$out"; then
    echo "  ❌ needle 校验失败：$out 不含 reasoning_details" >&2
    return 1
  fi
  # 新增的工具使用策略 prompt（0.5.2 引入）——证明 config.rs 改动已编入库
  if ! grep -qa "联网搜索只是" "$out"; then
    echo "  ❌ needle 校验失败：$out 不含新 system prompt（config.rs 未生效）" >&2
    return 1
  fi
  echo "  ✓ needle 校验通过（$ver_needle + reasoning_details + 新 prompt）"
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
