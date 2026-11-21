#!/bin/bash
# 編譯 release 並包成 dist/MeetingRecorder.app 與 dist/MeetingRecorder-macos-<arch>.zip。
# 交叉編譯：TARGET=x86_64-apple-darwin ./scripts/bundle.sh（Intel 版需要含 x86_64 的 SDK，例如 Xcode 26）
# 簽章：設 SIGN_IDENTITY="<鑰匙圈裡的程式碼簽章憑證名稱>" 用固定憑證（TCC 權限才不會每次重編就失效）；
# 沒設就用 ad-hoc 簽章（每次重編都要重新授權螢幕錄影）。
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$(brew --prefix rustup 2>/dev/null)/bin:$HOME/.cargo/bin:$PATH"
if [ -n "${TARGET:-}" ]; then
  cargo build --release --target "$TARGET"
  BIN="target/$TARGET/release/meeting-recorder"
else
  cargo build --release
  BIN="target/release/meeting-recorder"
fi
case "$(lipo -archs "$BIN")" in
  *x86_64*) ARCH=x86_64 ;;
  *) ARCH=arm64 ;;
esac
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=dist/MeetingRecorder.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/"
cp packaging/icons/AppIcon.icns "$APP/Contents/Resources/"
cp LICENSE "$APP/Contents/Resources/"
sed "s/__VERSION__/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"
codesign --force --options runtime --entitlements packaging/entitlements.plist \
  --sign "${SIGN_IDENTITY:--}" "$APP"
codesign --verify --verbose=2 "$APP"
# 打包成 zip 方便下載（保留簽章與屬性）
( cd dist && rm -f "MeetingRecorder-macos-${ARCH}.zip" && ditto -c -k --keepParent MeetingRecorder.app "MeetingRecorder-macos-${ARCH}.zip" )
echo "完成：${APP}（${ARCH}，版本 ${VERSION}，簽章：${SIGN_IDENTITY:-ad-hoc}）"
