# Supported `devcontainer.json` configuration

This doc tracks what the Devcontainers.app host actually understands today,
and how it maps each top-level field onto the Apple `container` CLI. Keep
it in sync with [src-tauri/crates/devcontainer-core/src/devcontainer/translate.rs](../src-tauri/crates/devcontainer-core/src/devcontainer/translate.rs)
and [src-tauri/crates/devcontainer-core/src/devcontainer/lifecycle.rs](../src-tauri/crates/devcontainer-core/src/devcontainer/lifecycle.rs).

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

Before any of the steps below, `up`/`stop`/`remove` first call
`runtime.ensure_system_running(...)`. For the Apple backend that
translates to a `container system status` probe and, if the daemon
isn't up, a `container system start` invocation whose log lines (
`container system: starting…` / `container system: started`) are
piped into the dashboard log pane. The result is cached in an
`AtomicBool` on the runtime so subsequent calls are free.

The same step also makes a one-time best-effort attempt to register
`host.docker.internal` as a localhost-redirect DNS domain. Apple's
`container` runtime has no built-in equivalent of Docker Desktop's
`host.docker.internal`; the documented mechanism is `sudo container
system dns create <domain> --localhost <ipv4-addr>`, which writes a
scoped resolver under `/etc/resolver/<domain>` and reloads
`mDNSResponder` plus a packet-filter rule that forwards `<ipv4-addr>`
back to `127.0.0.1` on the host. The backend:

1. runs `container system dns ls` (no privilege required) and skips
   out if `host.docker.internal` is already there;
2. otherwise shells out via `osascript -e 'do shell script "<container>
   system dns create host.docker.internal --localhost 203.0.113.113"
   with administrator privileges'`, surfacing the standard macOS auth
   dialog exactly once. The redirect IP is `203.0.113.113` from the
   RFC 5737 documentation range so it cannot collide with real
   networks.

The registration persists across host reboots until the user runs
`sudo container system dns delete host.docker.internal`. If the user
cancels the prompt or the command fails for any other reason the
backend logs a warning and continues — every other lifecycle op is
unaffected, only resolution of `host.docker.internal` from inside
containers will not work. An `AtomicBool` on the runtime guarantees
the prompt is shown at most once per process.

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

### Config-hash labels and cache-hit skips

Every `up` computes a SHA-256 over the bytes of `devcontainer.json`
plus, when `build:` is present, the bytes of the resolved `Dockerfile`.
(`build.context` is intentionally **not** walked — for a desktop app the
recursive-hash cost on a large repo isn't worth it; touch the
Dockerfile or devcontainer.json to force a rebuild of context-only
changes.) The hex digest is stamped onto two places:

- the **image** at build time, via `container build --label
  org.devcontainers.config_hash=<hex>`;
- the **container** at create time, via `container create --label
  org.devcontainers.config_hash=<hex>`.

`resolve_image` uses this label to short-circuit:

- For `build:` configs, `image_label(tag, LABEL_CONFIG_HASH)` is
  consulted before invoking `container build`. If the existing image
  carries the same hash we skip the build entirely and reuse the tag.
- For `image:` configs, `image_exists(ref)` is consulted before
  invoking `container image pull`. If the image is already local we
  skip the pull.

### Drift detection

`container_status` calls `inspect` on the live container (when one
exists), reads the stamped `org.devcontainers.config_hash` label off
`configuration.labels`, recomputes the hash from disk, and compares.
The result rides through to the WebView as
`ContainerStatus.configDrift`:

- `true` — labels disagree; dashboard shows a non-modal
  "devcontainer.json has changed since this container was created.
  Rebuild to apply." banner with an inline Rebuild button.
- `false` — labels match; no banner.
- omitted/`undefined` — undecidable (no live container, label missing
  on an adopted container, parsed config not yet submitted). UI treats
  this the same as `false`.

Drift inspection deliberately does **not** call
`ensure_system_running` — it's a poll, not a user action, so we don't
want it booting the daemon as a side effect. If `inspect` fails for any
reason drift is reported as undecidable.

### Repo action buttons

The per-repo header surfaces four buttons whose enablement is driven
by the live `ContainerStatus`:

| Button   | Meaning                                                                                  | Enabled when               |
| -------- | ---------------------------------------------------------------------------------------- | -------------------------- |
| Start    | `container_up` — pull/build if needed, create if needed, start.                          | not running, not transient |
| Stop     | `container_stop` — leaves the container around for a future Start.                       | running                    |
| Restart  | `container_stop` followed by `container_up` on the **same** container (no recreate).     | running                    |
| Rebuild  | `container_remove` + `container_up` — drops the instance, rebuilds image if hash drifts. | not transient              |

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
| `onCreateCommand`, `updateContentCommand`, `postCreateCommand` | **Create-time** hooks. Run once per container instance, in this order, immediately after the first successful create+start. We stamp `/var/devcontainer/postcreate_done` inside the container after they succeed; on subsequent starts (stop+start, or adoption-by-name on app boot) we probe that sentinel and skip these hooks if it exists. |
| `postStartCommand`, `postAttachCommand` | **Start-time** hooks. Run on every successful start, including after a plain `Start` of an already-created container. (`postAttachCommand` currently runs alongside `postStartCommand` until a real attach surface lands.) |
| `initializeCommand`                | Host-side hook. Not implemented in the MVP; deferred behind the Phase 2 permission model. |
| `features`                         | Parsed but not yet installed. OCI-fetch + install layer pending.      |
| `customizations`                   | Forwarded to the WebView; host ignores it.                            |
| `dockerComposeFile`                | **Rejected** with an actionable error message.                        |

## Tests guarding this behavior

In [src-tauri/crates/devcontainer-core/src/devcontainer/lifecycle.rs](../src-tauri/crates/devcontainer-core/src/devcontainer/lifecycle.rs) `mod tests`:

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

In [src-tauri/crates/devcontainer-core/src/container/apple_containers/cli.rs](../src-tauri/crates/devcontainer-core/src/container/apple_containers/cli.rs):

- `build_args_*` tests pin the exact argv ordering for `container build`,
  including deterministic `--build-arg` sort order.
