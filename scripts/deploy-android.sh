#!/usr/bin/env bash
# 把 ulnclaw 引擎的 libzclaw.so 部署到 Android App jniLibs（双 ABI）并校验。
set -euo pipefail

APP=/e/Users/Administrator/Desktop/omnimind-app/app/src/main/jniLibs
SRC=/e/srcode/ulnclaw-research/dist/android

for abi in arm64-v8a armeabi-v7a; do
  echo "==> $abi"
  if [ ! -f "$SRC/$abi/libzclaw.so" ]; then echo "  ❌ 源不存在 $SRC/$abi/libzclaw.so"; exit 1; fi
  before=$(stat -c %s "$APP/$abi/libzclaw.so" 2>/dev/null || echo 无)
  cp -f "$SRC/$abi/libzclaw.so" "$APP/$abi/libzclaw.so"
  echo "  旧=$before 新=$(stat -c %s "$APP/$abi/libzclaw.so")"
  ok=1
  for n in "0.6.1-ulnclaw-mobile" "cn.bing.com" "联网搜索只是"; do
    if grep -qa "$n" "$APP/$abi/libzclaw.so"; then echo "  ✓ $n"; else echo "  ❌ 缺 $n"; ok=0; fi
  done
  if grep -qa "0.5.2-mobile" "$APP/$abi/libzclaw.so"; then echo "  ❌ 仍含旧版串"; ok=0; else echo "  ✓ 无旧版串"; fi
  [ "$ok" = "1" ] || exit 1
done
echo
echo "=== ✅ Android 双 ABI 部署+校验通过（ulnclaw 引擎）==="
