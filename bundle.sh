#!/bin/sh
# Build target/recordr.app. Install with: cp -R target/recordr.app /Applications/
set -eu
cd "$(dirname "$0")"
cargo build --release
app=target/recordr.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp Info.plist "$app/Contents/"
mkdir -p "$app/Contents/Resources"
# Icon source is assets/icon.svg; regenerate the .icns with assets/make-icon.sh.
cp assets/AppIcon.icns "$app/Contents/Resources/"
cp target/release/recordr "$app/Contents/MacOS/"
# Sign with a stable local identity so macOS keeps the permissions across rebuilds
# (an ad-hoc signature changes on every build, and TCC forgets the grants).
# ponytail: a self-signed cert only works on this Mac; sharing needs Developer ID + notarization.
# RECORDR_SIGN=- signs ad-hoc (CI has no keychain identity).
id="${RECORDR_SIGN:-recordr local signing}"
if [ "$id" != "-" ] && ! security find-certificate -c "$id" >/dev/null 2>&1; then
  tmp=$(mktemp -d)
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$id" \
    -addext "extendedKeyUsage=critical,codeSigning" -addext "keyUsage=critical,digitalSignature" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null
  openssl pkcs12 -export -legacy -passout pass:recordr -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12"
  security import "$tmp/id.p12" -P recordr -T /usr/bin/codesign
  rm -rf "$tmp"
fi
codesign --force --sign "$id" "$app"
echo "built $app"
if [ "${1:-}" = "--dmg" ]; then
  rm -rf target/dmg target/recordr.dmg
  mkdir target/dmg && cp -R "$app" target/dmg/ && ln -s /Applications target/dmg/Applications
  hdiutil create -volname recordr -srcfolder target/dmg -format UDZO target/recordr.dmg
  echo "built target/recordr.dmg"
fi
