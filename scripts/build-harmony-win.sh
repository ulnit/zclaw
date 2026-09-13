#!/usr/bin/env bash
# 构建 OHOS (HarmonyOS) 版 libzclaw.so（ulnclaw 引擎，zclaw ABI）
# DevEco clang 是 shell 脚本，cargo 无法直接执行 → .cmd 包装（zclaw 已验证方案）。
set -euo pipefail

export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
cd /e/srcode/ulnclaw-research/mobile-ffi

CC='E:\srcode\ulnclaw-research\scripts\ohos-clang.cmd'
AR='E:\srcode\ulnclaw-research\scripts\ohos-ar.cmd'
export CC_aarch64_unknown_linux_ohos="$CC"
export AR_aarch64_unknown_linux_ohos="$AR"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$CC"

# 🔴 aws-lc-sys（rustls 默认 crypto provider，经 tokio-tungstenite/gateway 引入）
# 用 cmake 构建 C/汇编。Android 靠 cargo-ndk 自动注入 toolchain，OHOS 必须手动给。
#
# 关键三件套（实测踩坑顺序）：
#  1. CMAKE_TOOLCHAIN_FILE → DevEco 的 ohos.toolchain.cmake（内含 NDK clang 路径）。
#     缺它 cmake 拿宿主 MSVC cl.exe 去编 OHOS 目标 → D8021 无效参数。
#  2. CMAKE_GENERATOR=Ninja + ninja 在 PATH。Windows cmake 默认 "Visual Studio 17"
#     生成器，同样用 cl.exe。DevEco 不带 ninja，用 Android SDK 的
#     （/e/Android/Sdk/cmake/3.22.1/bin/ninja.exe，实测可用）。
#  3. 换生成器前必须清 target/<triple>/release/build/aws-lc-sys-* 缓存，
#     否则 CMakeCache.txt 记录着旧生成器 → "generator does not match" 报错。
# 好消息：cmake 命令自带 -DDISABLE_GO=ON -DDISABLE_PERL=ON，aws-lc-sys 用预生成
# 汇编，**不需要 Go/nasm/perl**，36.8s 即可编完。
export CMAKE_TOOLCHAIN_FILE='D:\Program Files\Huawei\DevEco Studio\sdk\default\openharmony\native\build\cmake\ohos.toolchain.cmake'
export OHOS_ARCH='arm64-v8a'
export CMAKE_GENERATOR=Ninja
export CMAKE_BUILD_TYPE=Release
export PATH="/e/Android/Sdk/cmake/3.22.1/bin:$PATH"

echo "==> cargo build --release --target aarch64-unknown-linux-ohos（fat LTO，约 10-20 分钟）"
cargo build --release --target aarch64-unknown-linux-ohos 2>&1 | grep -vE "^\s+Compiling|warning:" | tail -40

OUT=/e/srcode/ulnclaw-research/dist/harmony/arm64-v8a
mkdir -p "$OUT"
SO=target/aarch64-unknown-linux-ohos/release/libzclaw.so
if [ ! -f "$SO" ]; then echo "❌ 未产出 $SO"; exit 1; fi
cp -f "$SO" "$OUT/"
echo
echo "==> 产物"
ls -la "$OUT/libzclaw.so"

echo
echo "==> needle 校验"
ok=1
for n in "0.6.1-ulnclaw-mobile" "cn.bing.com" "联网搜索只是"; do
  if grep -qa "$n" "$OUT/libzclaw.so"; then
    echo "  ✓ 含: $n"
  else
    echo "  ❌ 缺: $n"
    ok=0
  fi
done
if grep -qa "0.5.2-mobile" "$OUT/libzclaw.so"; then
  echo "  ❌ 仍含旧 zclaw 版本串"
  ok=0
else
  echo "  ✓ 无旧 zclaw 版本串"
fi
echo
if [ "$ok" = "1" ]; then echo "=== ✅ OHOS .so 校验全部通过 ==="; else echo "=== ❌ 校验失败 ==="; exit 1; fi
