# Building Devcontainers.app on macOS

The Linux devcontainer in [.devcontainer/](../.devcontainer/) is set up to run
the same checks as [.github/workflows/devcontainers-app.yml](../.github/workflows/devcontainers-app.yml):
`cargo check` / `clippy` / `cargo test` against `src-tauri/`, `deno task
build` for the engine bundle, and `tsc --noEmit` for the [spec/](../spec/)
slice. It **cannot** produce a macOS `.app` — Tauri needs WebKit and the
Apple linker/codesign tooling, which only exist on macOS.

To actually run and try the app, build on the host Mac.

## 1. Install host prerequisites (one time)

```sh
scripts/mac-setup.sh
```

The script is idempotent and installs whatever is missing:

- Xcode Command Line Tools (`xcode-select --install`)
- Rust (must already be installed via `rustup` or `brew install rust`)
- Homebrew packages: `deno`, `yarn`, `sccache`
- `cargo-tauri` v2 (`cargo install tauri-cli --version "^2.0" --locked`)
- `RUSTC_WRAPPER=sccache` exported in `~/.zshrc` so every Tauri rebuild
  reuses cached crate compilations (the slow part of `cargo tauri build`).
  Check cache stats any time with `sccache --show-stats`.

## 2. Install JS deps + build the engine bundle

```sh
scripts/mac-bootstrap.sh
```

Equivalent to:

```sh
yarn install --frozen-lockfile
yarn --cwd frontend install --frozen-lockfile
deno task build
```

Re-run after changes to `package.json`, `frontend/package.json`,
`deno.json`, or the `spec/` slice.

## 3. Dev loop

```sh
scripts/mac-dev.sh
# == cd src-tauri && cargo tauri dev
```

This honors `tauri.conf.json`'s `beforeDevCommand` (`yarn --cwd ../frontend
dev` on `http://localhost:1420`), so Vite + the Rust host come up together
with hot reload.

## 4. Release build (`.app` / `.dmg`)

`tauri.conf.json` has `"bundle": { "active": false, ... }`, so bundle
targets must be passed explicitly:

```sh
scripts/mac-build.sh            # .app only
scripts/mac-build.sh app dmg    # .app + .dmg
```

Output: [src-tauri/target/release/bundle/](../src-tauri/target/release/bundle/).

For a universal binary:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
TARGET=universal-apple-darwin scripts/mac-build.sh
```

The bundle is unsigned. macOS Gatekeeper will refuse to launch it on first
run; right-click → **Open**, or:

```sh
xattr -dr com.apple.quarantine src-tauri/target/release/bundle/macos/Devcontainers.app
```

Real signing/notarization is out of scope until later in the roadmap.

## When to use the devcontainer instead

Use [.devcontainer/](../.devcontainer/) when you want to reproduce the CI
matrix — Rust lints, Deno bundle, spec typecheck — in a clean Linux env.
Use the macOS host scripts above when you want to **run** the app.
