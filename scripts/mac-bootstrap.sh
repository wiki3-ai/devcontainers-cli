#!/usr/bin/env bash
#
# mac-bootstrap.sh — install JS deps and produce the engine bundle.
#
# Run once after `scripts/mac-setup.sh`, and again whenever package.json,
# frontend/package.json, deno.json, or the spec/ slice change.

set -euo pipefail

cd "$(dirname "$0")/.."

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

# yarn classic on Node >= 22 occasionally hits an EEXIST race when creating
# nested node_modules directories in parallel. --network-concurrency 1
# serializes fetch+link and avoids it. If you still hit it, delete the
# affected node_modules dir and re-run this script.
YARN_FLAGS=(--frozen-lockfile --network-concurrency 1)

log "yarn install (root — for spec slice tsc)"
yarn install "${YARN_FLAGS[@]}"

log "yarn install (frontend — Vite)"
yarn --cwd frontend install "${YARN_FLAGS[@]}"

log "deno task build (engine bundle → dist/devcontainer-engine.js)"
deno task build

log "install git hooks (rustfmt pre-commit)"
scripts/install-git-hooks.sh

log "Done. Next: scripts/mac-dev.sh (dev) or scripts/mac-build.sh (release .app)"
