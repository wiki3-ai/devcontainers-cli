# Supported `devcontainer.json` configuration

This doc tracks what the Devcontainers.app host actually understands today,
and how it maps each top-level field onto the Apple `container` CLI. Keep
it in sync with [src-tauri/src/devcontainer/translate.rs](../src-tauri/src/devcontainer/translate.rs)
and [src-tauri/src/devcontainer/lifecycle.rs](../src-tauri/src/devcontainer/lifecycle.rs).

## Container model: one container per repo

The app is intentionally **one container per workspace folder**. A
"workspace" here is usually a wiki/repo, but can also be a long-running
service (agents, databases, ML inference, …). This shapes a few choices:

- `dockerComposeFile` is **explicitly rejected** with a clear error. We do
  not plan to add Compose support — multi-container topologies should be
  expressed as multiple workspaces, each with its own `devcontainer.json`,
  composed at the app/dashboard level rather than at the container layer.
- The synthesized image tag for `build`-based configs is
  `devcontainer-<workspace-slug>:latest`, scoped to the workspace.
- Container names are derived from the workspace folder name via
  `sanitize_entity_name`.

## Image source: `image` *or* `build` (exactly one)

### `image: "<ref>"`

Pulled with `container image pull <ref>` before create. Parsed via
`parse_image_ref` so registry / repository / tag / digest are tracked
separately.

### `build: { ... }`

Built locally with `container build` before create. Supported sub-fields:

| Field        | Behavior                                                               |
| ------------ | ---------------------------------------------------------------------- |
| `dockerfile` | Path **relative to `.devcontainer/`**. Defaults to `Dockerfile`.       |
| `context`    | Path **relative to `.devcontainer/`**. Defaults to `.` (the `.devcontainer/` folder itself). |
| `args`       | Forwarded as `--build-arg KEY=VALUE`. Sorted for deterministic argv.   |
| `target`     | Forwarded as `--target <stage>`.                                       |
| `cacheFrom`  | Parsed but not yet forwarded — Apple `container build` has no flag.    |

Path resolution mirrors the upstream spec: both `dockerfile` and `context`
are anchored at the directory containing `devcontainer.json`. So in
`/repo/.devcontainer/devcontainer.json`:

```jsonc
{ "build": { "dockerfile": "Dockerfile", "context": ".." } }
```

…builds from `/repo/` with `--file /repo/.devcontainer/Dockerfile`.

The resolved `configFilePath` is forwarded from the JS engine bundle
(`frontend/src/devcontainer-engine/index.ts`) to Rust through
`ParsedDevContainer.config_file_path`, which is what the lifecycle uses
to anchor the relative paths.

### Neither `image` nor `build`

Validation rejects the config. There is no silent fallback to a default
base image — the previous `mcr.microsoft.com/devcontainers/base:ubuntu`
default has been removed.

## Lifecycle: build vs pull dispatch

`LifecycleOrchestrator::resolve_image` decides per-workspace what to do:

```
parsed.build.is_some()  →  emit "building" status
                            container build --tag devcontainer-<slug>:latest \
                                            --file <abs dockerfile> \
                                            [--build-arg k=v]... \
                                            [--target stage] \
                                            <abs context>
                            stream stdout/stderr line-by-line into the
                            dashboard log pane via LogChunk
                            on success → use the synthesized tag
parsed.image  →             emit "pulling" status
                            container image pull <ref>
                            on success → use the parsed ImageRef
```

After `resolve_image` returns an `ImageRef`, `to_container_spec` takes
that ref as a parameter — image resolution and spec translation are no
longer entangled.

### Failure surfaces

`apple_containers::build` captures stderr while it streams it. On a
non-zero exit, the error wraps the full `container build …` argv plus
the trimmed stderr, so the dashboard error pill shows what went wrong
without forcing the user to scroll the log.

The `building` status flows through the `EventSink` seam exactly like
`pulling`, so xterm-side rendering and the test `CapturingSink` see the
same events.

## Other top-level fields

| Field                              | State                                                                 |
| ---------------------------------- | --------------------------------------------------------------------- |
| `name`                             | Used to derive container name; sanitized.                             |
| `workspaceFolder`, `workspaceMount`| Mapped onto a host bind mount of the workspace dir.                   |
| `mounts`                           | Forwarded to `container run --mount`.                                 |
| `containerEnv`, `remoteEnv`        | Forwarded as `--env KEY=VALUE`.                                       |
| `runArgs`                          | Appended verbatim to `container run`.                                 |
| `forwardPorts`                     | Tracked, surfaced in the UI; no automatic publish yet.                |
| `postCreateCommand`, `postStartCommand`, `postAttachCommand`, `initializeCommand`, `onCreateCommand`, `updateContentCommand` | Run via `portable-pty`; output streamed to the log pane. |
| `features`                         | Parsed but not yet installed. OCI-fetch + install layer pending.      |
| `customizations`                   | Forwarded to the WebView; host ignores it.                            |
| `dockerComposeFile`                | **Rejected** with an actionable error message.                        |

## Tests guarding this behavior

In [src-tauri/src/devcontainer/lifecycle.rs](../src-tauri/src/devcontainer/lifecycle.rs) `mod tests`:

- `up_emits_pulling_creating_running_in_order` — happy path for `image`.
- `up_pull_failure_surfaces_error_status_and_log` — pull failure path.
- `up_with_dockerfile_build_invokes_runtime_build_and_emits_building_status`
  — happy path for `build`, asserts the resolved Dockerfile/context paths
  and the `devcontainer-<slug>:latest` tag, plus state sequence
  `building → creating → created → running`.
- `up_with_build_failure_surfaces_error_status_and_log` — build failure
  path: error message includes captured stderr, error status emitted.
- `up_with_compose_config_reports_unsupported_error` — compose rejection
  message stays stable.

In [src-tauri/src/container/apple_containers/cli.rs](../src-tauri/src/container/apple_containers/cli.rs):

- `build_args_*` tests pin the exact argv ordering for `container build`,
  including deterministic `--build-arg` sort order.
