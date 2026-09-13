#!/usr/bin/env bash
# 构建 mobile-ffi (libzclaw.so) for Android via cargo-ndk。
# 🔴 产物名故意保持 libzclaw.so —— App 端 JNI dlopen("libzclaw.so")
# 与 Kotlin/ArkTS 解析器零改动，换引擎对 App 层透明。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && { pwd -W 2>/dev/null || pwd; })"
cd "$REPO_ROOT/mobile-ffi"

NDK="${ANDROID_NDK_HOME:-/opt/android-ndk}"
export ANDROID_NDK_HOME="$NDK"
OUT_DIR="$REPO_ROOT/dist/android"
mkdir -p "$OUT_DIR/arm64-v8a" "$OUT_DIR/armeabi-v7a"

# needle：版本串（区分旧 zclaw 库）+ 引擎标识 + Bing 源 + 移动端 prompt
NEEDLES=("0.6.1-ulnclaw-mobile" "cn.bing.com" "联网搜索只是" "ulnclaw")

verify() {
  local out="$1"
  local ok=1
  for n in "${NEEDLES[@]}"; do
    if grep -qa "$n" "$out"; then
      echo "  ✓ $n"
    else
      echo "  ❌ 缺: $n"
      ok=0
    fi
  done
  # 旧 zclaw 独有串必须不存在（证明装的是新引擎不是旧库）
  if grep -qa "0.5.2-mobile" "$out"; then
    echo "  ❌ 仍含旧版串 0.5.2-mobile（装错库了）"
    ok=0
  else
    echo "  ✓ 无旧 zclaw 版本串"
  fi
  [ "$ok" = "1" ]
}

build_one() {
  local target="$1" abi="$2" api="${3:-24}"
  echo "==> building $target ($abi, api $api)"
  cargo ndk --platform "$api" -t "$abi" -o "$OUT_DIR/$abi" build --release
  local out="$OUT_DIR/$abi/libzclaw.so"
  local built="target/$target/release/libzclaw.so"
  # cargo ndk 的 -o 在 MSYS 下可能不真拷贝（zclaw 教训）——时间戳兜底
  if [ -f "$built" ] && [ "$built" -nt "$out" ]; then
    echo "  ⚠️ cargo ndk 未拷贝 → 手动 cp"
    cp -f "$built" "$out"
  fi
  ls -la "$out"
  verify "$out"
}

case "${1:-all}" in
  arm64)  build_one aarch64-linux-android arm64-v8a ;;
  armv7)  build_one armv7-linux-androideabi armeabi-v7a ;;
  all|*)  build_one aarch64-linux-android arm64-v8a
          build_one armv7-linux-androideabi armeabi-v7a ;;
esac
echo
echo "Done: $OUT_DIR/<abi>/libzclaw.so（ulnclaw 引擎，zclaw ABI）"
