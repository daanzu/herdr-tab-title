#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN_DIR="$ROOT/bin"
BIN="$BIN_DIR/herdr-tab-title"
REPO=${HERDR_TAB_TITLE_REPO:-daanzu/herdr-tab-title}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/herdr-plugin.toml" | head -n 1)

mkdir -p "$BIN_DIR"

restart_needed=0
tmp=""

find_watcher_pid() {
  ps -ax -o pid= -o command= 2>/dev/null |
    awk -v bin="$BIN" '
      {
        pid = $1
        command = $0
        sub(/^[[:space:]]*[0-9]+[[:space:]]+/, "", command)
        if (command == bin " watch" || index(command, bin " watch ") == 1) {
          print pid
          exit
        }
      }'
}

stop_existing_watcher() {
  pid=$(find_watcher_pid)
  if [ -z "$pid" ]; then
    return 0
  fi

  restart_needed=1
  echo "stopping existing tab title watcher: pid $pid"
  if ! kill "$pid" 2>/dev/null && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  waited=0
  while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "watcher did not stop; forcing termination: pid $pid" >&2
    kill -KILL "$pid"
  fi
}

start_watcher_if_needed() {
  if [ "$restart_needed" -ne 1 ]; then
    return 0
  fi

  if [ -n "${HERDR_PLUGIN_STATE_DIR:-}" ]; then
    "$BIN" start
  elif command -v herdr >/dev/null 2>&1; then
    herdr plugin action invoke daanzu.tab-title.start
  else
    "$BIN" start
  fi
  restart_needed=0
}

cleanup() {
  [ -z "$tmp" ] || rm -f "$tmp"
  if [ "$restart_needed" -eq 1 ]; then
    start_watcher_if_needed || echo "warning: could not restart tab title watcher" >&2
  fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

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
    chmod +x "$tmp"
    stop_existing_watcher
    mv -f "$tmp" "$BIN"
    start_watcher_if_needed
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

# Build into a temporary file, then replace the installed binary only after
# the old watcher has stopped. The rename also avoids partial installations.
tmp="$BIN.tmp.$$"
cp "$ROOT/target/release/herdr-tab-title" "$tmp"
chmod +x "$tmp"
stop_existing_watcher
mv -f "$tmp" "$BIN"
start_watcher_if_needed
echo "built $BIN"
