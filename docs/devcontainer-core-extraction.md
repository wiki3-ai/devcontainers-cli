# Extracting `devcontainer-core` for reuse across Tauri apps

## Why

Two sibling Tauri 2 desktop apps need the same dev-container lifecycle
behavior:

- **devcontainers-cli** (this repo) — the reference implementation. Has
  the runtime-agnostic [`ContainerRuntime`][trait] trait, Apple Containers /
  Podman / Docker backends, devcontainer.json → `ContainerSpec` translation,
  and a [`LifecycleOrchestrator`][orch] that drives build/pull/create/start
  with config-hash labels, drift detection, hook execution, and event
  emission.
- **wiki3-app** (`/Users/jim/Projects/wiki3-app`) — a knowledge-garden
  desktop app that needs to manage one dev container per garden. Today it
  ships a minimal subset (`src-tauri/src/tools/`) with detection, an
  in-process QuickJS parser for `devcontainer.json`, and a ~100-line
  build/pull helper. No labels, no drift, no hooks.

The goal is **one source of truth** for the container model and
lifecycle, consumed by both apps as a Rust library, with no copy/paste
and no submodules.

[trait]: ../src-tauri/src/container/traits.rs
[orch]: ../src-tauri/src/devcontainer/lifecycle.rs

## Design: a Cargo workspace with a reusable core crate

devcontainers-cli grows a Cargo *workspace* containing two members:

1. `devcontainer-core` — the reusable library (`rlib`), no Tauri dep.
2. `devcontainers-app` — the Tauri 2 binary (unchanged from the
   user's perspective).

wiki3-app depends on `devcontainer-core` via a path dependency during
co-development, swapping to a git revision dep for reproducible
releases.

```
devcontainers-cli/
  src-tauri/
    Cargo.toml                       # [workspace] root + bin crate
    crates/
      devcontainer-core/             # NEW reusable library crate
        Cargo.toml
        src/
          lib.rs
          events.rs                  # EventSink trait (no tauri)
          container/                 # moved from src-tauri/src/container/
            mod.rs traits.rs
            apple_containers/ docker.rs podman.rs
          devcontainer/              # moved from src-tauri/src/devcontainer/
            mod.rs translate.rs lifecycle.rs
    src/                             # binary crate stays here
      lib.rs main.rs
      commands/                      # unchanged; uses devcontainer_core::*
      host/  pty/                    # app-specific
      tauri_sink.rs                  # impl EventSink for TauriSink (~20 lines)
```

We deliberately keep the workspace root inside `src-tauri/` rather than
at the repo root. Tauri's `tauri.conf.json` and `build.rs` already
assume `src-tauri/` is the cargo root; declaring `[workspace]` there
avoids touching any Tauri build plumbing.

### What goes in `devcontainer-core` (no `tauri` dependency)

| Module | Source today | Notes |
|---|---|---|
| `container::traits` | `src-tauri/src/container/traits.rs` | as-is |
| `container::apple_containers` | `src-tauri/src/container/apple_containers/` | as-is — already `tauri`-free |
| `container::docker`, `container::podman` | same | as-is |
| `container::RuntimeRegistry` | `src-tauri/src/container/mod.rs` | as-is |
| `devcontainer::translate` | same | as-is |
| `devcontainer::lifecycle` | same | strip `TauriSink`; keep `EventSink` trait, `LifecycleOrchestrator`, `LifecycleStatus`, `LifecycleError`, `LABEL_CONFIG_HASH`, hash + drift logic |
| `events::EventSink` | extracted from `lifecycle.rs` | trait stays here, no impls |

Crate dependencies (subset of what `src-tauri/Cargo.toml` already pulls
in): `serde`, `serde_json`, `thiserror`, `anyhow`, `tokio`,
`async-trait`, `tracing`, `parking_lot`, `once_cell`, `uuid`, `url`,
`percent-encoding`, `sha2`, `hex`. Notably **not** `tauri`,
`tauri-plugin-*`, `dirs`, `tracing-subscriber` — those stay in the
binary.

### What stays in `devcontainers-cli/src-tauri/src/` (the binary)

- `commands/*` — Tauri command surface (calls into `devcontainer_core`).
- `host/*` — workspace registry, persistence, native menu, permissions
  (product-specific).
- `pty/*` — terminal hookup.
- `tauri_sink.rs` — `impl EventSink for TauriSink<'a>(&'a AppHandle)`,
  the only Tauri-coupled bit that previously lived in `lifecycle.rs`.
- `lib.rs`, `main.rs` — unchanged shape; imports flip from
  `crate::container::…` to `devcontainer_core::container::…`.

### What changes in wiki3-app

`wiki3-app/src-tauri/Cargo.toml` adds:

```toml
[dependencies]
devcontainer-core = { path = "../../devcontainers-cli/src-tauri/crates/devcontainer-core" }
async-trait = "0.1"
parking_lot = "0.12"
sha2 = "0.10"
hex = "0.4"
```

(Path resolves from `/Users/jim/Projects/wiki3-app/src-tauri/` to
`/Users/jim/Projects/devcontainers-cli/src-tauri/crates/devcontainer-core`.
This relies on the sibling-checkout layout the user actually has in
their VS Code workspace.)

Then in `wiki3-app/src-tauri/src/`:

1. **Delete** `tools/apple_container.rs` and `tools/devcontainer_image.rs`.
   Callers switch to `devcontainer_core::container::apple_containers::{detect, AppleContainersRuntime}`
   and `devcontainer_core::devcontainer::lifecycle::LifecycleOrchestrator`.
2. **Keep** `tools/devcontainer_config.rs` + `.js` for now — wiki3-app
   parses `devcontainer.json` server-side with embedded QuickJS, while
   devcontainers-cli parses it in the WebView's TypeScript spec engine.
   Add a small adapter `fn into_parsed(self) -> devcontainer_core::ParsedDevContainer`.
   Convergence is a separate decision (see Phase 4).
3. **Add** `Wiki3Sink: devcontainer_core::EventSink` (~20 lines, mirrors
   `TauriSink`) emitting whatever event names wiki3's frontend wants.
   The IPC surface stays per-app; only the *core* is shared.
4. **Add** Tauri command wrappers (`devcontainer_up`, `devcontainer_stop`,
   `devcontainer_status`, …) that hold a managed `LifecycleOrchestrator`
   and `RuntimeRegistry`, mirroring what
   [`commands/lifecycle.rs`](../src-tauri/src/commands/lifecycle.rs) does
   in this repo.

### Why this shape (alternatives considered)

| Option | Verdict |
|---|---|
| Cargo workspace + path/git crate dep ✅ | Idiomatic Rust. Single source of truth. Each app keeps its own commands, state, UI, permissions, trust model. Cross-repo iteration via `path =` is normal. |
| Git submodule pulling devcontainers-cli into wiki3-app | Would make wiki3-app build the whole CLI binary it doesn't need; murky upgrade story. |
| Single Tauri app with two windows | Ruled out — separately-shipped products with different UX and trust models. |
| Publish `devcontainer-core` to crates.io | Premature; revisit once the API has settled across both apps. |
| Copy/paste | Explicitly rejected. |

## Phasing

### Phase 1 — Refactor in-place inside devcontainers-cli

Pure mechanical move; **no behavior change, no API change visible to
the WebView**.

1. Convert `src-tauri/Cargo.toml` to a workspace root with one initial
   member (`crates/devcontainer-core`).
2. Create `crates/devcontainer-core/{Cargo.toml,src/lib.rs}`.
3. Move `src-tauri/src/container/` → `crates/devcontainer-core/src/container/`.
4. Move `src-tauri/src/devcontainer/` → `crates/devcontainer-core/src/devcontainer/`.
5. Extract the `EventSink` trait into `crates/devcontainer-core/src/events.rs`.
   Re-export from `lib.rs`.
6. Move the `TauriSink` struct + impl from `lifecycle.rs` into
   `src-tauri/src/tauri_sink.rs`.
7. In `src-tauri/Cargo.toml`, add `devcontainer-core = { path = "crates/devcontainer-core" }`
   and drop the dependencies that now live only in the core crate (none
   actually — the binary still uses some).
8. Update imports in `src-tauri/src/{lib.rs,commands/*}` from
   `crate::container::…` / `crate::devcontainer::…` to
   `devcontainer_core::…`.
9. Run `cargo test -p devcontainers-app -p devcontainer-core` and the
   existing Mocha integration tests. The unit tests in
   `container/apple_containers/cli.rs::tests` and
   `devcontainer/lifecycle.rs::tests` should pass unchanged.

Ship/verify before touching wiki3-app.

### Phase 2 — Prove the API by consuming it from wiki3-app

1. Add the path dep to `wiki3-app/src-tauri/Cargo.toml`.
2. Write `Wiki3Sink: EventSink` and one Tauri command (`devcontainer_up`).
3. Replace `tools/devcontainer_image.rs` with a call into the
   orchestrator behind that command.
4. Validate against an existing wiki3 garden repo with a real
   `.devcontainer/devcontainer.json`.

This is also when we shake out anything in `devcontainer-core`'s public
API that turned out to be too tightly bound to devcontainers-cli's
specific event shapes or workspace model.

### Phase 3 — Migrate the rest of wiki3-app

1. Replace `tools/apple_container.rs` detection with
   `devcontainer_core::container::apple_containers::detect()`.
2. Add the remaining lifecycle commands (`stop`, `rebuild`, `remove`,
   `status` with drift).
3. Surface drift / labels / hooks in the wiki3 dashboard UI.

### Phase 4 (optional, later) — Converge devcontainer.json parsing

Two parsers exist today:

- **devcontainers-cli**: TypeScript spec engine in the WebView
  (`frontend/src/devcontainer-engine/`) that posts a `ParsedDevContainer`
  JSON to Rust.
- **wiki3-app**: in-process QuickJS module
  (`tools/devcontainer_config.{rs,js}`) that parses on the Rust side.

Both target the same `ParsedDevContainer` shape after Phase 1 lands.
Convergence options:

- Move QuickJS parsing into `devcontainer-core` so devcontainers-cli can
  drop the WebView spec engine.
- Move the TS spec engine output format into `devcontainer-core` (no
  parser change) and let wiki3-app keep using QuickJS as one valid
  producer of `ParsedDevContainer`.

Don't block phases 1–3 on this.

## Cross-repo development workflow (for future reference)

Because wiki3-app uses `path = "../../devcontainers-cli/..."`, day-to-day:

- Edit `devcontainer-core` in either VS Code workspace; both apps see
  changes immediately on next `cargo build`.
- Run tests in devcontainers-cli's `src-tauri/` for the canonical core
  test suite; wiki3-app only tests its own integration code.
- For releases, pin wiki3-app to a specific git rev:

  ```toml
  devcontainer-core = { git = "https://github.com/.../devcontainers-cli", rev = "abc1234" }
  ```

  This decouples wiki3-app's release cadence from devcontainers-cli's
  unreleased main.

- Cargo workspace lock files: each Tauri app has its own `Cargo.lock`
  (workspace-scoped). The two apps can resolve different versions of
  shared transitive deps without conflict.

## Public API of `devcontainer-core` (initial sketch)

```rust
// crates/devcontainer-core/src/lib.rs
pub mod container;     // ContainerRuntime, ContainerSpec, ImageRef, …
pub mod devcontainer;  // ParsedDevContainer, LifecycleOrchestrator, …
pub mod events;        // EventSink

pub use container::{
    ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerStatus,
    ImageRef, RuntimeId, RuntimeRegistry,
};
pub use devcontainer::lifecycle::{
    LifecycleError, LifecycleOrchestrator, LifecycleStatus, LABEL_CONFIG_HASH,
};
pub use devcontainer::translate::{ParsedDevContainer, DevContainerBuild};
pub use events::EventSink;
```

This is what the Phase 1 PR ratifies; later phases may add to it but
should not break it.
