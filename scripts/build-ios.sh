#!/usr/bin/env bash
# 在 Mac 上构建 mobile-ffi 的 iOS 静态库（libzclaw-device.a + libzclaw-sim.a）。
# 🔴 必须在 Mac 跑：iOS target 需要 Apple SDK。
# 产物名沿用 zclaw（libzclaw-*.a），project.yml 的 OTHER_LDFLAGS(-lzclaw-device/-sim)
# 与 ZClawBridge.h 零改动即可链接。
#
# 前置：把 ulnclaw-research/{src,mobile-ffi} 同步到 Mac 同样相对结构
#       （mobile-ffi 通过 path = "../src" 依赖 ulnclaw）。
set -euo pipefail
cd "$(dirname "$0")/../mobile-ffi"

OUT_DIR="$(pwd)/../dist/ios"
mkdir -p "$OUT_DIR"

rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios 2>/dev/null || true

echo "==> device (arm64)"
cargo build --release --target aarch64-apple-ios
echo "==> simulator (arm64)"
cargo build --release --target aarch64-apple-ios-sim
echo "==> simulator (x86_64)"
cargo build --release --target x86_64-apple-ios

cp target/aarch64-apple-ios/release/libzclaw.a "$OUT_DIR/libzclaw-device.a"
lipo -create \
  target/aarch64-apple-ios-sim/release/libzclaw.a \
  target/x86_64-apple-ios/release/libzclaw.a \
  -output "$OUT_DIR/libzclaw-sim.a" 2>/dev/null \
  || cp target/aarch64-apple-ios-sim/release/libzclaw.a "$OUT_DIR/libzclaw-sim.a"

echo
echo "==> needle 校验（确认是 ulnclaw 引擎，非旧 zclaw）"
ok=1
for lib in libzclaw-device.a libzclaw-sim.a; do
  for n in "0.6.1-ulnclaw-mobile" "联网搜索只是"; do
    if grep -qa "$n" "$OUT_DIR/$lib"; then echo "  ✓ $lib 含 $n"; else echo "  ❌ $lib 缺 $n"; ok=0; fi
  done
done
echo
ls -la "$OUT_DIR"
[ "$ok" = "1" ] && echo "=== ✅ iOS 静态库构建+校验通过 ===" || { echo "=== ❌ 校验失败 ==="; exit 1; }
