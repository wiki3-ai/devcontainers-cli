#!/usr/bin/env bash
# Compile perry-bridge/src/entry.ts to a native binary using Perry.
#
# This script is INDEPENDENT from the existing Node CLI build pipeline. It
# does not touch esbuild.js, dist/, or built/. Running `npm run compile`
# / `npm run package` continues to behave exactly as it did before.
#
# Usage:
#   scripts/build-perry.sh            # build for the host triple
#   scripts/build-perry.sh <triple>   # build for a specific Rust triple
#
# Output: perry-bridge/rust/devcontainer-config/bin/devcontainer-config-<triple>

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Read the pinned Perry version (informational only — the actual `perry` on
# PATH is what we run; we do not auto-install).
# shellcheck disable=SC1091
source perry-bridge/PERRY_VERSION

if ! command -v perry >/dev/null 2>&1; then
	cat >&2 <<EOF
perry CLI not found on PATH.

Install Perry version ${PERRY_VERSION} per its upstream instructions and
re-run this script. We do not auto-install Perry from this repository — the
Node CLI build does not depend on it, and developers who never touch
perry-bridge/ should not be forced to install it.
EOF
	exit 127
fi

triple="${1:-$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')}"
if [ -z "$triple" ]; then
	echo >&2 "could not detect host triple; pass one explicitly: $0 <triple>"
	exit 2
fi

out_dir="perry-bridge/rust/devcontainer-config/bin"
mkdir -p "$out_dir"

case "$triple" in
	*windows*) ext=".exe" ;;
	*)         ext=""     ;;
esac

out="$out_dir/devcontainer-config-$triple$ext"

echo "perry compile -> $out"
perry compile \
	--target "$triple" \
	--tsconfig perry-bridge/tsconfig.perry.json \
	-o "$out" \
	perry-bridge/src/entry.ts

echo "ok: $out"
