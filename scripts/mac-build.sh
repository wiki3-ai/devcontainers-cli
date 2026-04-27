#!/usr/bin/env bash
#
# mac-build.sh — build a release .app (and optional .dmg) on macOS.
#
# tauri.conf.json currently has "bundle.active": false, so we pass
# --bundles explicitly. Output lands in src-tauri/target/release/bundle/.
#
# Usage:
#   scripts/mac-build.sh                # .app only (default)
#   scripts/mac-build.sh app dmg        # .app + .dmg
#   TARGET=universal-apple-darwin scripts/mac-build.sh
#
# For a universal binary, first run:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin

set -euo pipefail

cd "$(dirname "$0")/../src-tauri"

bundles=("${@:-app}")

args=(tauri build --bundles "${bundles[@]}")
if [[ -n "${TARGET:-}" ]]; then
	args+=(--target "$TARGET")
fi

exec cargo "${args[@]}"
