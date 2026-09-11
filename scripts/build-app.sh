#!/usr/bin/env bash
# Build the Rust binary and assemble a signed DSH.app bundle.
# Usage: scripts/build-app.sh [--zip]
set -euo pipefail
cd "$(dirname "$0")/.."

# Cargo home stays inside the project so builds never touch ~/.cargo.
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"

# Regenerate icons when missing.
if [ ! -f assets/icon.icns ] || [ ! -f assets/tray-template.rgba ]; then
  python3 scripts/gen_icons.py
fi

echo "==> cargo build --release"
cargo build --release

APP=build/DSH.app
echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/dsh-client "$APP/Contents/MacOS/"
cp assets/icon.icns "$APP/Contents/Resources/"
cp packaging/Info.plist "$APP/Contents/"

echo "==> ad-hoc codesign"
codesign --force --deep -s - "$APP"
codesign --verify --deep --strict "$APP" 2>/dev/null && echo "signature OK"

echo "Built: $APP"
echo "  binary size: $(du -h target/release/dsh-client | cut -f1)"

if [ "${1:-}" = "--zip" ]; then
  echo "==> creating zip (ditto, no resource forks: keeps the codesign seal intact)"
  rm -f build/DSH.zip
  ditto -c -k --norsrc --keepParent "$APP" build/DSH.zip
  echo "Built: build/DSH.zip"
fi
