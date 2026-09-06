#!/bin/sh
# Assemble JLocal.app from a compiled jlocal binary + committed packaging assets.
# Usage: BIN=path/to/jlocal TAG=v0.1.0 ./packaging/make-app.sh [OUTPUT_DIR]
# Defaults: BIN=$BIN or target/aarch64-apple-darwin/release/jlocal or
#   target/release/jlocal; TAG=$TAG or 0.0.0; OUTPUT_DIR=dist.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

BIN="${BIN:-}"
if [ -z "$BIN" ]; then
  for candidate in \
    "$SCRIPT_DIR/../target/aarch64-apple-darwin/release/jlocal" \
    "$SCRIPT_DIR/../target/release/jlocal"; do
    if [ -x "$candidate" ]; then BIN="$candidate"; break; fi
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "error: jlocal binary not found. Build first (cargo build --release) or set BIN=." >&2
  exit 1
fi

TAG="${TAG:-0.0.0}"
OUT_DIR="${1:-$SCRIPT_DIR/../dist}"
APP="$OUT_DIR/JLocal.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/jlocal"
chmod +x "$APP/Contents/MacOS/jlocal"
cp "$SCRIPT_DIR/jlocal.icns" "$APP/Contents/Resources/jlocal.icns"

sed -e "s/@TAG@/$TAG/g" "$SCRIPT_DIR/Info.plist.template" > "$APP/Contents/Info.plist"

echo "Built $APP (binary: $BIN, version: $TAG)"
