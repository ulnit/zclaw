#!/usr/bin/env bash
# 把 ulnclaw 引擎的 OHOS libzclaw.so 部署到 Harmony 工程两处并校验。
#   entry/src/main/cpp/libs/arm64-v8a/  ← CMake POST_BUILD 源
#   entry/libs/arm64-v8a/                ← hvigor 收集打进 HAP
set -euo pipefail

HARM=/e/Users/Administrator/Desktop/omnimind-harmony
SRC=/e/srcode/ulnclaw-research/dist/harmony/arm64-v8a/libzclaw.so
[ -f "$SRC" ] || { echo "❌ 源不存在 $SRC（先跑 build-harmony-win.sh）"; exit 1; }

for dst in "$HARM/entry/src/main/cpp/libs/arm64-v8a" "$HARM/entry/libs/arm64-v8a"; do
  echo "==> $dst"
  mkdir -p "$dst"
  before=$(stat -c %s "$dst/libzclaw.so" 2>/dev/null || echo 无)
  cp -f "$SRC" "$dst/libzclaw.so"
  echo "  旧=$before 新=$(stat -c %s "$dst/libzclaw.so")"
  ok=1
  for n in "0.6.2-ulnclaw-mobile" "cn.bing.com" "联网搜索只是"; do
    if grep -qa "$n" "$dst/libzclaw.so"; then echo "  ✓ $n"; else echo "  ❌ 缺 $n"; ok=0; fi
  done
  if grep -qa "0.5.2-mobile" "$dst/libzclaw.so"; then echo "  ❌ 仍含旧版串"; ok=0; else echo "  ✓ 无旧版串"; fi
  [ "$ok" = "1" ] || exit 1
done
echo
echo "=== ✅ Harmony 部署+校验通过（ulnclaw 引擎）==="
