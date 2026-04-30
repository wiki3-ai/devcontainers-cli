#!/usr/bin/env bash
#
# install-git-hooks.sh — point this checkout's git hooks at scripts/git-hooks/.
#
# Idempotent: re-running just resets `core.hooksPath` to the same value.
# Skips silently if not inside a git work tree (e.g. if the source has
# been extracted from a tarball without a .git directory).

set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -d .git ] && ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "install-git-hooks: not a git checkout; skipping"
    exit 0
fi

# Use core.hooksPath rather than copying files into .git/hooks/ so that
# updates to scripts/git-hooks/ propagate without a re-install, and so
# the hooks are visible/diffable in the repo itself.
git config core.hooksPath scripts/git-hooks
chmod +x scripts/git-hooks/* 2>/dev/null || true

echo "install-git-hooks: core.hooksPath -> scripts/git-hooks"
