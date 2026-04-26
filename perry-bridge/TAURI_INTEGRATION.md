# Embedding `perry-bridge` in a Tauri 2 application

Two integration shapes are supported. Pick **one**.

## Shape 1 — Sidecar binary (recommended for production)

This is what `wiki3-app` should ship. Tauri's bundler picks up the
Perry-compiled binary as an `externalBin`, signs it as part of the app bundle,
and launches it on demand. No `include_bytes!`, no extracting to tempdir, no
quarantine prompts on macOS.

1. Build the binary on each target platform (Linux, macOS x64/arm64, Windows
   x64) using `scripts/build-perry.sh` from this repo. CI matrix is the
   simplest path; see `.github/workflows/perry.yml`.

2. Copy the produced binaries into your Tauri app's `src-tauri/binaries/`
   directory, **named with the Rust target triple suffix** Tauri expects:
   ```
   src-tauri/binaries/devcontainer-config-x86_64-unknown-linux-gnu
   src-tauri/binaries/devcontainer-config-aarch64-apple-darwin
   src-tauri/binaries/devcontainer-config-x86_64-apple-darwin
   src-tauri/binaries/devcontainer-config-x86_64-pc-windows-msvc.exe
   ```

3. Declare the sidecar in `src-tauri/tauri.conf.json`:
   ```json
   {
     "bundle": {
       "externalBin": ["binaries/devcontainer-config"]
     }
   }
   ```

4. In `src-tauri/Cargo.toml`, add this crate as a path or git dependency:
   ```toml
   [dependencies]
   devcontainer-config = { git = "https://github.com/wiki3-ai/devcontainers-cli", branch = "main", default-features = false, features = ["sidecar"] }
   ```
   The `sidecar` feature disables the embedded-binary path (see Shape 2) and
   makes the crate look for the binary at the path supplied by the caller.

5. In your command handler:
   ```rust
   #[tauri::command]
   async fn parse_devcontainer(
       app: tauri::AppHandle,
       workspace: String,
   ) -> Result<serde_json::Value, String> {
       let sidecar = app
           .shell()
           .sidecar("devcontainer-config")
           .map_err(|e| e.to_string())?;
       let bin = sidecar.program().to_path_buf();
       devcontainer_config::load_devcontainer_config_with_binary(
           &bin,
           std::path::Path::new(&workspace),
           None,
       )
       .await
       .map_err(|e| e.to_string())
   }
   ```

## Shape 2 — Embedded binary (recommended for development)

For quick iteration the crate can `include_bytes!` the Perry binary and
extract it to a temp dir on first call. This avoids the Tauri bundling step
but does not survive code-signing well on macOS in production. Build the
crate with `--features embed` (default on); the Perry binary must be present
under `perry-bridge/rust/devcontainer-config/bin/devcontainer-config-<triple>`
at `cargo build` time.

```toml
[dependencies]
devcontainer-config = { git = "..." }   # default features include "embed"
```

```rust
#[tauri::command]
async fn parse_devcontainer(workspace: String) -> Result<serde_json::Value, String> {
    devcontainer_config::load_devcontainer_config(std::path::Path::new(&workspace))
        .await
        .map_err(|e| e.to_string())
}
```

## Returning typed results to the front-end

`load_devcontainer_config` returns `serde_json::Value` whose shape matches
`DevContainerConfig` from `src/spec-configuration/configuration.ts`. You can
either:

- forward it as-is to the front-end (it serialises cleanly), or
- declare typed Rust structs and `#[derive(Deserialize)]` them; the field
  names match the JSON one-to-one.

We do not generate Rust types from `configuration.ts` automatically. The TS
surface is small and the manual mapping costs less than dragging in
`ts-rs`/`schemars`/etc.

## Threading model

The crate's public API is `async`. Internally it spawns the Perry binary with
`tokio::process::Command` and services host calls on the same task. Multiple
concurrent calls from Tauri are fine — each spawns a fresh subprocess.

## Updating the slice

When a new release of this CLI changes any module under
`src/spec-configuration/`, `src/spec-common/variableSubstitution.ts`, or
`src/spec-utils/workspaces.ts`, re-run:

```sh
./scripts/build-perry.sh
cargo test -p devcontainer-config
```

The `re-exports.ts` file imports those modules verbatim; type or shape drift
will surface as a TS compile error rather than a silent regression.
