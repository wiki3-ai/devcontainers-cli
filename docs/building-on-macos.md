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

This honors `tauri.conf.json`'s `beforeDevCommand` (`yarn --cwd frontend
dev` on `http://localhost:1420`), so Vite + the Rust host come up together
with hot reload.

To crank up Rust-side logging, set `RUST_LOG` before launching:

```sh
RUST_LOG=devcontainers_app_lib=debug scripts/mac-dev.sh
```

Lifecycle stages (`pull`, `create`, `start`, `exec`, hooks) emit `tracing`
events at `info`, with the `container` CLI's stderr captured at `error`
on failure. The same details are forwarded to the WebView as
`devcontainer://log` events so they show up in the in-app terminal.

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

## Notes for future-us

### How the WebView loads the engine bundle

The deno-built spec slice (`dist/devcontainer-engine.js`) is copied by
[scripts/build-engine.ts](../scripts/build-engine.ts) into
`frontend/public/devcontainer-engine.js`. Files under `frontend/public/`
are served verbatim by Vite (and copied verbatim into `frontend/dist/`
at build time), which is what we want — the engine bundle has its own
Node polyfills baked in via esbuild, and we don't want Vite/Rollup
rewriting any of it.

Vite 7 deliberately refuses to **`import()`** modules out of `/public/`,
even with `/* @vite-ignore */`, because public assets are not meant to go
through the module pipeline. So [frontend/src/main.ts](../frontend/src/main.ts)
`fetch()`s the bundle as text, wraps it in a `Blob`, and dynamic-imports
the resulting `blob:` URL. The blob URL is opaque to Vite and the
browser treats it as a fully-formed ES module.

If you ever need to tighten the Tauri WebView CSP in
[src-tauri/tauri.conf.json](../src-tauri/tauri.conf.json), make sure
`script-src` keeps `blob:` (or `'unsafe-inline'` is already broad enough
to include it via the dev `csp_dev` override). Without it, the engine
dynamic import will fail in the packaged release build with a CSP
violation.
