#!/usr/bin/env bash
#
# mac-dev.sh — run Devcontainers.app in dev mode on macOS.
#
# Pipeline:
#   1. Build the deno-bundled engine module once (so the WebView has an
#      up-to-date `/devcontainer-engine.js` on first load).
#   2. Start `deno task build:watch` in the background so edits under
#      `spec/` and `frontend/src/devcontainer-engine/` rebuild the bundle
#      automatically. Vite's dev server picks up the new file on its
#      next request — no manual restart.
#   3. Exec `cargo tauri dev`, which runs the Vite frontend (via
#      tauri.conf.json's beforeDevCommand) and the Rust host together.
#
# The watcher is killed when this script exits.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

# 1. One-shot build so the bundle exists before the WebView loads.
echo "==> Building engine bundle (one-shot)…"
deno task build

# 2. Start the watcher in the background. Send its output to stderr so
#    it doesn't get tangled with cargo tauri's stdout, but keep it
#    visible.
echo "==> Starting engine watcher…"
deno task build:watch >&2 &
watcher_pid=$!

# Make sure the watcher dies with us (Ctrl-C, exit, or error).
cleanup() {
	if kill -0 "$watcher_pid" 2>/dev/null; then
		kill "$watcher_pid" 2>/dev/null || true
		wait "$watcher_pid" 2>/dev/null || true
	fi
}
trap cleanup EXIT INT TERM

# 3. Run the Tauri dev loop in the foreground.
cd src-tauri
exec cargo tauri dev "$@"
