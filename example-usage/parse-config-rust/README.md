# `parse-config-rust`

Minimal Cargo example showing how to parse a `devcontainer.json` from a Rust
host with **no** Node.js / Deno / Bun installed, by spawning the
[`perry-bridge`](../../perry-bridge/) native binary.

## What this example *does* replace

Nothing in the existing examples list. The other folders under
`example-usage/` (`tool-vscode-server`, `tool-openvscode-server`,
`tool-vim-via-ssh`, `image-build`, `ci-app-build-script`) all exercise
container *runtime* operations (`devcontainer up` / `build` / `exec`) that are
explicitly out of scope for `perry-bridge` — they need Docker, OCI registry
auth, PTY, tarball streaming, and `proxy-agent`, none of which the
`perry-bridge` slice covers.

This example demonstrates the only thing `perry-bridge` is built to do:

> Read `.devcontainer/devcontainer.json` from a workspace folder, JSONC-parse
> it, apply the "old property" upgrade, and resolve pre-container variable
> substitutions.

## Pre-requisites

1. A working Rust toolchain (`rustup`, `cargo`).
2. The `perry-bridge` native binary built for your host triple. From the
   repository root:
   ```sh
   ./scripts/build-perry.sh
   ```
   This writes `perry-bridge/rust/devcontainer-config/bin/devcontainer-config-<triple>`.
   The `parse-config-rust` build embeds it via the
   `devcontainer-config` crate's `embed` feature.

## Running

```sh
cd example-usage/parse-config-rust
cargo run
```

Expected output: a pretty-printed JSON object whose `config.name` matches the
`name` field in `../workspace/.devcontainer/devcontainer.json`.

## Acceptance test

`tests/golden.rs` runs the same flow and diffs key fields against a fixed
list. If the workspace `devcontainer.json` is updated, regenerate by running
`cargo test -- --nocapture` and copying the printed output into the test.

## Failure mode without the Perry binary

If `scripts/build-perry.sh` was never run, `cargo run` exits non-zero with
`error: perry binary not found at <path>; run scripts/build-perry.sh`. This
is intentional — the crate compiles either way so CI on machines without
Perry still passes its non-Perry tests.
