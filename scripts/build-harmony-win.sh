#!/usr/bin/env bash
# 构建 OHOS (HarmonyOS) 版 libzclaw.so 0.5.2
# Windows 下 DevEco 的 clang 是 shell 脚本，cargo 无法直接执行 → 用 .cmd 包装。
set -euo pipefail

export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
cd /e/srcode/zclaw/rust/zclaw

CC='E:\srcode\zclaw\scripts\ohos-clang.cmd'
AR='E:\srcode\zclaw\scripts\ohos-ar.cmd'
export CC_aarch64_unknown_linux_ohos="$CC"
export AR_aarch64_unknown_linux_ohos="$AR"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER="$CC"

echo "==> cargo build --release --target aarch64-unknown-linux-ohos"
cargo build --release --target aarch64-unknown-linux-ohos 2>&1 | grep -vE "^\s+Compiling|warning:" | tail -12

OUT=/e/srcode/zclaw/dist/harmony/arm64-v8a
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
for n in "0.5.2-mobile" "reasoning_details" "联网搜索只是"; do
  if grep -qa "$n" "$OUT/libzclaw.so"; then
    echo "  ✓ 含: $n"
  else
    echo "  ❌ 缺: $n"
    ok=0
  fi
done
# 新收口提示的独有片段（0.5.2 引入）
if grep -qa "停止调用任何工具" "$OUT/libzclaw.so"; then
  echo "  ✓ 含: 停止调用任何工具（新 COLLAR_PROMPT）"
else
  echo "  ❌ 缺: 停止调用任何工具"
  ok=0
fi
# 🔴 v0.5.1 的旧收口提示文案必须已被替换掉（0.5.2 换成了 COLLAR_PROMPT）。
# 早期版本这里误写成「必须含有」，导致校验假失败。
if grep -qa "已达到本轮工具调用上限" "$OUT/libzclaw.so"; then
  echo "  ❌ 仍含 v0.5.1 旧收口提示（应已替换为 COLLAR_PROMPT）"
  ok=0
else
  echo "  ✓ v0.5.1 旧收口提示已替换"
fi
# 旧的道歉兜底文案必须已被移除（仅在 COLLAR_PROMPT 的「禁止使用」引用里出现「抱歉，尝试」）
echo
echo "==> 旧道歉兜底残留检查"
if grep -qa "本轮尝试了多次检索仍未拿到" "$OUT/libzclaw.so"; then
  echo "  ❌ 仍含旧道歉兜底文案（未清除）"
  ok=0
else
  echo "  ✓ 旧道歉兜底文案已移除"
fi
echo
if [ "$ok" = "1" ]; then echo "=== ✅ OHOS .so 校验全部通过 ==="; else echo "=== ❌ 校验失败 ==="; exit 1; fi
