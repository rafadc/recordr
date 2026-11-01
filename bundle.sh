#!/bin/sh
# Build target/recordr.app. Install with: cp -R target/recordr.app /Applications/
set -eu
cd "$(dirname "$0")"
cargo build --release
app=target/recordr.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp Info.plist "$app/Contents/"
cp target/release/recordr "$app/Contents/MacOS/"
# Ad-hoc signature: enough for macOS to remember the Audio Capture permission locally.
# ponytail: sharing with other Macs needs a Developer ID signature + notarization.
codesign --force --sign - "$app"
echo "built $app"
