#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
BIN_NAME="rccb"

cd "$ROOT_DIR"
cargo build --release

mkdir -p "$DIST_DIR"
cp "$ROOT_DIR/target/release/$BIN_NAME" "$DIST_DIR/$BIN_NAME"
chmod +x "$DIST_DIR/$BIN_NAME"

echo "deliverable: $DIST_DIR/$BIN_NAME"
