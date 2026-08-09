#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN_DIR="$ROOT/bin"
BIN="$BIN_DIR/herdr-tab-title"
REPO=${HERDR_TAB_TITLE_REPO:-daanzu/herdr-tab-title}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/herdr-plugin.toml" | head -n 1)

mkdir -p "$BIN_DIR"

os=$(uname -s)
arch=$(uname -m)

case "$os:$arch" in
  Linux:x86_64)
    asset="herdr-tab-title-x86_64-unknown-linux-gnu"
    ;;
  Darwin:x86_64)
    asset="herdr-tab-title-x86_64-apple-darwin"
    ;;
  Darwin:arm64)
    asset="herdr-tab-title-aarch64-apple-darwin"
    ;;
  *)
    asset=""
    ;;
esac

if [ -n "$asset" ] && command -v curl >/dev/null 2>&1; then
  url="https://github.com/$REPO/releases/download/v$VERSION/$asset"
  tmp="$BIN.download"
  if curl -fsL "$url" -o "$tmp"; then
    mv "$tmp" "$BIN"
    chmod +x "$BIN"
    echo "installed $asset"
    exit 0
  fi
  rm -f "$tmp"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "no release binary for this platform and cargo is not available" >&2
  exit 127
fi

cd "$ROOT"
cargo build --release
cp "$ROOT/target/release/herdr-tab-title" "$BIN"
chmod +x "$BIN"
echo "built $BIN"
