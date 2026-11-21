#!/bin/bash
# 編譯 release 並包成 dist/MeetingRecorder.app。
# 簽章：設 SIGN_IDENTITY="<鑰匙圈裡的程式碼簽章憑證名稱>" 用固定憑證（TCC 權限才不會每次重編就失效）；
# 沒設就用 ad-hoc 簽章（每次重編都要重新授權螢幕錄影）。
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$(brew --prefix rustup 2>/dev/null)/bin:$HOME/.cargo/bin:$PATH"
cargo build --release
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=dist/MeetingRecorder.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/meeting-recorder "$APP/Contents/MacOS/"
sed "s/__VERSION__/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
codesign --force --options runtime --entitlements packaging/entitlements.plist \
  --sign "${SIGN_IDENTITY:--}" "$APP"
codesign --verify --verbose=2 "$APP"
echo "完成：${APP}（版本 ${VERSION}，簽章：${SIGN_IDENTITY:-ad-hoc}）"
