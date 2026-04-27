#!/usr/bin/env bash
#
# mac-setup.sh — one-time host setup for building Devcontainers.app on macOS.
#
# The repo's devcontainer (under .devcontainer/) runs Linux and is fine for
# `cargo check` / `clippy`, `deno task build`, and the spec-slice `tsc`
# checks — i.e. what the GitHub Actions matrix in
# .github/workflows/devcontainers-app.yml runs. It cannot produce a macOS
# .app bundle. For that, build directly on the host Mac.
#
# This script installs the prerequisites that are missing on a fresh Mac:
#   * Xcode Command Line Tools (linker, codesign, system SDK)
#   * Homebrew packages: deno, yarn
#   * cargo-tauri CLI v2
#
# Rust itself is assumed to be already installed (rustup or Homebrew).
# Re-running is safe; each step is idempotent.

set -euo pipefail

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }

if [[ "$(uname -s)" != "Darwin" ]]; then
	warn "This script is for macOS. On Linux, use the devcontainer in .devcontainer/."
	exit 1
fi

# 1. Xcode Command Line Tools
if ! xcode-select -p >/dev/null 2>&1; then
	log "Installing Xcode Command Line Tools (a GUI prompt will appear)…"
	xcode-select --install || true
	warn "Re-run this script once the Command Line Tools install completes."
	exit 1
else
	log "Xcode Command Line Tools: $(xcode-select -p)"
fi

# 2. Rust
if ! command -v cargo >/dev/null 2>&1; then
	warn "cargo not found. Install Rust via https://rustup.rs/ or 'brew install rust', then re-run."
	exit 1
fi
log "Rust: $(rustc --version)"

# 3. Homebrew (required for deno + yarn install below)
if ! command -v brew >/dev/null 2>&1; then
	warn "Homebrew not found. Install from https://brew.sh/ and re-run."
	exit 1
fi

# 4. deno
if ! command -v deno >/dev/null 2>&1; then
	log "Installing deno via Homebrew…"
	brew install deno
else
	log "deno: $(deno --version | head -1)"
fi

# 5. yarn (classic — repo uses yarn.lock)
if ! command -v yarn >/dev/null 2>&1; then
	log "Installing yarn via Homebrew…"
	brew install yarn
else
	log "yarn: $(yarn --version)"
fi

# 6. cargo-tauri CLI (v2)
if ! cargo tauri --version >/dev/null 2>&1; then
	log "Installing cargo-tauri (this can take a few minutes)…"
	cargo install tauri-cli --version "^2.0" --locked
else
	log "cargo-tauri: $(cargo tauri --version)"
fi

# 7. sccache — caches compiled Rust crates across builds. Big win for Tauri,
# whose dep graph is large and otherwise rebuilt from scratch in fresh
# target/ dirs (e.g. after `cargo clean` or on CI).
if ! command -v sccache >/dev/null 2>&1; then
	log "Installing sccache via Homebrew…"
	brew install sccache
else
	log "sccache: $(sccache --version)"
fi

# Persist RUSTC_WRAPPER=sccache in ~/.zshrc so every shell (including new
# VS Code terminals) picks it up. Idempotent.
ZSHRC="${ZDOTDIR:-$HOME}/.zshrc"
if ! grep -q 'RUSTC_WRAPPER=sccache' "$ZSHRC" 2>/dev/null; then
	log "Adding 'export RUSTC_WRAPPER=sccache' to $ZSHRC"
	printf '\n# Cache Rust compilation across projects (added by mac-setup.sh).\nexport RUSTC_WRAPPER=sccache\n' >> "$ZSHRC"
	warn "Open a new terminal (or 'source $ZSHRC') for RUSTC_WRAPPER to take effect."
else
	log "RUSTC_WRAPPER=sccache already configured in $ZSHRC"
fi

log "Host prerequisites OK. Next: scripts/mac-bootstrap.sh"
