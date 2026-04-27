#!/usr/bin/env bash
#
# mac-dev.sh — run Devcontainers.app in dev mode on macOS.
#
# Uses tauri.conf.json's beforeDevCommand to bring up the Vite frontend on
# http://localhost:1420 and launches the Tauri host with hot reload.

set -euo pipefail

cd "$(dirname "$0")/../src-tauri"

exec cargo tauri dev "$@"
