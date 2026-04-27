# `spec/` — Devcontainer spec slice

This is the **canonical TypeScript devcontainer-spec slice** that survives the
conversion of `devcontainers-cli` into the Tauri 2 *Devcontainers.app*. It is
the only TS code that runs at app runtime (inside the WebView, after Deno
transpiles it to a single ES module — see `deno.json` and
`frontend/src/devcontainer-engine/`).

## Scope

Just *load + parse + pre-container substitute* of `devcontainer.json`. That
covers exactly:

- `spec-configuration/configuration.ts`
- `spec-configuration/configurationCommonUtils.ts`
- `spec-configuration/editableFiles.ts`
- `spec-common/variableSubstitution.ts`
- `spec-common/errors.ts` (a slim version with no `injectHeadless`/`log` edges)
- `spec-utils/workspaces.ts`
- `spec-utils/pfs.ts` (a `FileHost`-interface-only file — no Node `fs`/`ncp`)

Anything that drags in Docker, git, OCI, `tar`, `ncp`, `node-pty`,
`proxy-agent`, etc. is **out of scope** here and lives in Rust under
`src-tauri/`.

## Out of scope (deliberately)

- `spec-configuration/containerFeaturesConfiguration.ts` — drags in `tar`. The
  Features pipeline is reimplemented in Rust (Phase 2).
- `spec-configuration/containerFeaturesOCI*.ts`, `containerTemplates*.ts`,
  `httpOCIRegistry.ts`, `lockfile.ts`, `featureAdvisories.ts`,
  `controlManifest.ts`, `containerCollections*.ts` — Rust.
- All of `spec-node/` — Rust.
- `spec-common/cliHost.ts`, `commonUtils.ts`, `dotfiles.ts`, `git.ts`,
  `injectHeadless.ts`, `proc.ts`, `shellServer.ts` — Node-host machinery; the
  Tauri Rust host replaces all of it.
- `spec-utils/httpRequest.ts`, `log.ts`, `product.ts`, `event.ts`,
  `strings.ts` — handled in Rust or replaced with thin frontend equivalents.

## Filesystem access

All FS operations go through the `FileHost` interface
(`spec-utils/pfs.ts`). At app runtime, the WebView-side adapter implements
`FileHost` by invoking Tauri commands (`fs_read_file`, `fs_stat`, …) which the
Rust host services with sandboxed access scoped to the active workspace.

## Relationship to `src/`

During the migration `src/` still holds the legacy Node CLI. The legacy code
is not deleted in this PR (that is step 9 of `ROADMAP.md`); it remains the
source of truth for tests and for any consumer still on the old toolchain.
The files under `spec/` are the new home and are intentionally a small,
Deno-buildable, FileHost-only copy.
