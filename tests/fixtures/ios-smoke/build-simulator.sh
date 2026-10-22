#!/bin/bash
# UNEXECUTED on macOS. Build only; no install, launch, signing or licence consent.
set -euo pipefail

test "$(uname -s)" = Darwin
test "$(uname -m)" = arm64
fixture="$(cd "$(dirname "$0")" && pwd)"
cd "$fixture"
mkdir -p out
{
  echo 'Synthetic simulator build only; unsigned; not a device IPA or runtime test'
  git rev-parse HEAD
  git status --porcelain
  sw_vers
  uname -sm
  xcode-select -p
  xcodebuild -version
  xcrun --sdk iphonesimulator --show-sdk-version
  xcrun swift --version
} > out/BUILD-INFO.txt

xcodebuild \
  -project IOSSmoke.xcodeproj \
  -scheme IOSSmoke \
  -configuration Debug \
  -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$fixture/out/DerivedData" \
  -jobs 2 \
  CODE_SIGNING_ALLOWED=NO ARCHS=arm64 ONLY_ACTIVE_ARCH=YES \
  build

app="$fixture/out/DerivedData/Build/Products/Debug-iphonesimulator/IOSSmoke.app"
test -f "$app/IOSSmoke"
/usr/bin/ditto -c -k --keepParent "$app" "$fixture/out/IOSSmoke.app.zip"
(cd out && shasum -a 256 IOSSmoke.app.zip > SHA256SUMS)
