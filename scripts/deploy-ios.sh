#!/usr/bin/env bash
# iOS 部署 + 链接验证：把 ulnclaw 静态库拷进 App，同步 Swift 改动，
# 跑 xcodebuild device Release 验证「Rust LLVM22 对象能否被 Xcode ld 链接」。
# 在 Windows 侧通过 ssh 驱动 Mac。
set -euo pipefail

MAC="mac-mini"
APP="~/Projects/app/omnimind-ios"
BUILD="~/ulnclaw-build"

echo "=== 1. 拷静态库进 App 的 ZClaw/Libs ==="
ssh -o ConnectTimeout=30 "$MAC" bash -s <<'EOF'
set -e
mkdir -p ~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs
# 优先用 build-ios.sh 的 dist 产物（含 lipo 合并的 sim）；没有就用 target 直出
if [ -f ~/ulnclaw-build/dist/ios/libzclaw-device.a ]; then
  cp -f ~/ulnclaw-build/dist/ios/libzclaw-device.a ~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs/
  echo "  ✓ device 来自 dist"
else
  cp -f ~/ulnclaw-build/mobile-ffi/target/aarch64-apple-ios/release/libzclaw.a ~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs/libzclaw-device.a
  echo "  ✓ device 来自 target（dist 未就绪）"
fi
if [ -f ~/ulnclaw-build/dist/ios/libzclaw-sim.a ]; then
  cp -f ~/ulnclaw-build/dist/ios/libzclaw-sim.a ~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs/
  echo "  ✓ sim 来自 dist"
fi
ls -la ~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs/
EOF

echo
echo "=== 2. needle 校验 .a 是 ulnclaw 引擎 ==="
ssh -o ConnectTimeout=30 "$MAC" bash -s <<'EOF'
A=~/Projects/app/omnimind-ios/OmniMind/ZClaw/Libs/libzclaw-device.a
for n in "0.6.0-ulnclaw-mobile" "联网搜索只是"; do
  if grep -qa "$n" "$A" 2>/dev/null; then echo "  ✓ 含 $n"; else echo "  ❌ 缺 $n"; fi
done
EOF

echo
echo "=== 3. 同步 Swift 改动到 Mac ==="
cd /e/Users/Administrator/Desktop/omnimind-ios
tar cf - --exclude='.git' --exclude='build' --exclude='Pods' --exclude='Vendor' \
  --exclude='*.xcodeproj' --exclude='ZClaw/Libs' \
  OmniMind/Data/ZClawNativeEngine.swift \
  OmniMind/Data/ZClawAgent.swift \
  OmniMind/ZClaw/Sources/ZClawBridge.mm \
  OmniMind/UI/Screens/ZClawChatView.swift \
  | ssh -o ConnectTimeout=30 "$MAC" "cd ~/Projects/app/omnimind-ios && tar xf - && echo '  ✓ Swift 改动已同步'"

echo
echo "=== 4. xcodegen + device Release 构建（验证链接）==="
ssh -o ConnectTimeout=40 "$MAC" bash -s <<'EOF'
export PATH=$HOME/bin:$PATH
cd ~/Projects/app/omnimind-ios
xcodegen generate 2>&1 | tail -2
echo "--- 开始 device Release 构建（CODE_SIGNING_ALLOWED=NO 仅验证链接）---"
xcodebuild -project OmniMind.xcodeproj -scheme OmniMind -configuration Release \
  -destination 'generic/platform=iOS' CODE_SIGNING_ALLOWED=NO build 2>&1 \
  | grep -E 'error:|Undefined symbol|ld:|BUILD FAILED|BUILD SUCCEEDED' | sort -u | head -40
EOF
