# Roadmap — `devcontainers-cli` → Devcontainers.app

This document tracks the conversion from the Node-based devcontainer CLI to a
Tauri 2 macOS app that manages dev containers. The high-level plan is in the
issue body; the steps below are the unit of PR-by-PR delivery.

## Phase 1 — Foundation (this PR)

- [x] **Step 1 — Carve out the spec slice** (`spec/`) with a `FileHost`-only
      `pfs.ts`. No edges into `containerFeaturesConfiguration` (`tar`),
      `pfs.ts` real impl (`ncp`), or any `spec-node` module.
- [x] **Step 2 — Deno build pipeline** (`deno.json`, `scripts/build-engine.ts`,
      `scripts/check-no-node-builtins.ts`) producing
      `dist/devcontainer-engine.js`. CI bans Node built-ins from the bundle.
- [x] **Step 3 — Tauri 2 scaffold** (`src-tauri/`) and Vite TS frontend
      (`frontend/`). `beforeBuildCommand` invokes `deno task build && yarn
      --cwd ../frontend build`.
- [x] **Step 4 — Host layer skeletons** (`host/{config,permissions,
      window_state,menu}.rs`, `commands/`) ported from
      `wiki3-ai/wiki3-app`.

## Phase 1 (cont.) — MVP

- [x] **Step 5 — `apple_containers` impl**: `pull`, `create`, `start`, `exec`,
      `logs`, `stop`, `remove`. Behind feature flag `apple-containers-live`
      for tests that drive the real `container` CLI.
- [x] **Step 6 — FileHost bridge wired end-to-end**: WebView-side adapter
      (`frontend/src/devcontainer-engine/index.ts`) calls Tauri commands
      that read a real `.devcontainer/devcontainer.json`; the spec slice
      returns a fully substituted config to Rust.
- [x] **Step 7 — Lifecycle orchestrator** in Rust: parsed config →
      `ContainerSpec` → `ContainerRuntime`. Run lifecycle hooks via
      `portable-pty`, stream logs/events to the WebView.
- [x] **Step 8 — MVP UI**: dashboard, "Open folder", workspace detail with
      Up/Stop/Rebuild/Terminal/Logs (xterm.js).
- [x] **Step 8b — `build:` support**: `ContainerRuntime::build()` plus an
      Apple Containers impl that shells out to `container build`, streams
      stdout/stderr into the dashboard log pane, and surfaces captured
      stderr on failure. Lifecycle dispatches build-vs-pull and emits a
      `building` status. Paths in `build.dockerfile` / `build.context`
      resolve relative to `.devcontainer/`, matching the upstream spec.
      Synthesized tag is `devcontainer-<workspace-slug>:latest`. See
      [docs/devcontainer-config-support.md](docs/devcontainer-config-support.md).
- [x] **Step 8c — `dockerComposeFile` policy**: explicitly rejected with
      an actionable error. The app's container model is **one container
      per workspace folder** (wikis, agents, databases, ML inference,
      …); multi-container topologies are composed at the dashboard
      level, not via Compose. This is a **product decision, not a
      deferral** — Step 10's compose bullet is removed.

## Phase 1 cleanup

- [ ] **Step 9 — Remove the legacy Node CLI**: delete `devcontainer.js`,
      `esbuild.js`, `azure-pipelines.yml`, `node-pty`, `proxy-agent`,
      `follow-redirects`, `yargs`, `chalk`, `src/spec-node/`, the old
      `src/spec-*` folders (now mirrored in `spec/`). Update `README.md`
      and rename the package.

## Phase 2 — Feature parity beyond MVP

- [ ] **Step 10 — Features + templates**: replace the Node `tar`/OCI logic
      with Rust (`oci-distribution`, `tar`, `flate2`). Templates browser.
      Dotfiles bootstrap.

## Phase 3 — Other backends and platforms

- [ ] **Step 11 — Podman, then Docker** backends. Non-mac platforms.

## Open questions to resolve before each phase

- Apple Containers programmatic API surface (Swift/C) — keep an eye out so
  `apple_containers.rs` can switch from CLI shelling to direct API calls.
- OCI auth parity with the upstream CLI for Features.
- Sync cadence with `devcontainers/cli`; the spec slice's directory layout
  is intentionally close to upstream so periodic merges are cheap.
