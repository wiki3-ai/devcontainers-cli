//! Translate a parsed `devcontainer.json` (received from the WebView spec
//! engine) into a [`ContainerSpec`] suitable for any
//! [`crate::container::ContainerRuntime`]. Only the v1 subset of fields is
//! consumed; later phases enrich it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::container::{ContainerSpec, ImageRef, MountKind, MountSpec};

/// `devcontainer.json` lifecycle command: either a single string parsed by
/// the shell or an explicit argv array. Mirrors the upstream schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LifecycleCommand {
    Single(String),
    Multiple(Vec<String>),
}

impl LifecycleCommand {
    /// Render as the argv passed to `runtime.exec(...)`. Single strings are
    /// run via `/bin/sh -c` so `&&`, redirects, etc. work; we deliberately
    /// avoid `-lc` (login shell) so the hook does not pick up unrelated
    /// profile scripts in the workspace image.
    pub fn to_argv(&self) -> Vec<String> {
        match self {
            LifecycleCommand::Single(s) => {
                vec!["/bin/sh".to_string(), "-c".to_string(), s.clone()]
            }
            LifecycleCommand::Multiple(parts) => parts.clone(),
        }
    }
}

/// Dockerfile-based build. Mirrors the upstream
/// `DevContainerFromDockerfileConfig.build` object. Paths are *not*
/// resolved here — they are interpreted relative to
/// [`ParsedDevContainer::config_file_path`]'s parent (the
/// `.devcontainer/` folder), per the upstream spec.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevContainerBuild {
    /// Dockerfile path relative to the `.devcontainer/` folder. Defaults
    /// to `Dockerfile` when omitted, matching the spec.
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Build context directory relative to the `.devcontainer/` folder.
    /// Defaults to `.` (the `.devcontainer/` folder itself).
    #[serde(default)]
    pub context: Option<String>,
    /// `--build-arg` values.
    #[serde(default)]
    pub args: std::collections::HashMap<String, String>,
    /// Multi-stage build target.
    #[serde(default)]
    pub target: Option<String>,
    /// Image tag(s) the upstream spec asks the build to be tagged with.
    /// We honour these when set; otherwise we synthesise a workspace-
    /// scoped tag.
    #[serde(default)]
    pub cache_from: Vec<String>,
}

/// Subset of the parsed `devcontainer.json` fields the v1 host consumes.
/// This is the contract sent from the WebView to the Rust host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedDevContainer {
    #[serde(default)]
    pub name: Option<String>,
    /// Pre-built image reference. Mutually exclusive with `build` per
    /// the spec; if both are set, `build` wins (matches upstream).
    #[serde(default)]
    pub image: Option<String>,
    /// Dockerfile-based build config. When set, the host will run
    /// `runtime.build(...)` to produce the image before creating the
    /// container.
    #[serde(default)]
    pub build: Option<DevContainerBuild>,
    /// Carried through so the host can surface a clear error. v1 does
    /// not implement compose orchestration (one-container-per-repo).
    #[serde(default, rename = "dockerComposeFile")]
    pub docker_compose_file: Option<serde_json::Value>,
    /// Absolute path to the `devcontainer.json` that produced this
    /// struct. Used to resolve build paths relative to its parent.
    #[serde(default)]
    pub config_file_path: Option<PathBuf>,
    #[serde(default)]
    pub workspace_folder: Option<PathBuf>,
    #[serde(default)]
    pub workspace_mount: Option<String>,
    /// devcontainer.json `overrideCommand`.
    ///
    /// Per the spec this defaults to `true`, which means the
    /// implementation replaces the image's inherited `ENTRYPOINT` /
    /// `CMD` with a long-running no-op so the container stays alive for
    /// `exec`-driven workflows. When a project explicitly sets
    /// `overrideCommand: false` the image's own `CMD` must be preserved
    /// — e.g. a project whose Dockerfile ends with
    /// `CMD ["gateway", "run"]`.
    ///
    /// `None` means "not specified" and is treated as `true`.
    #[serde(default)]
    pub override_command: Option<bool>,
    /// Raw `devcontainer.json` `mounts` entries, in declaration order.
    ///
    /// Both syntaxes the upstream implementations accept are carried
    /// through verbatim: the key/value form
    /// (`source=hermes-opt-data,target=/opt/data,type=volume`) and the
    /// short form (`./cache:/cache`). [`to_container_spec`] parses them
    /// into [`crate::container::MountSpec`]s; an entry we cannot
    /// interpret is reported as an error rather than silently dropped.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Verbatim docker-style flags from devcontainer.json `runArgs`.
    /// Forwarded as-is to the runtime's `create` invocation so users
    /// can request resource limits (`--cpus=4`, `--memory=8g`),
    /// extra capabilities, etc. Per the upstream spec these are
    /// passed to `docker run` exactly as written.
    #[serde(default)]
    pub run_args: Vec<String>,
    /// Ports to forward from the container to the host. The
    /// devcontainer spec allows entries to be either bare integers
    /// (`8000`) or strings (`"8000"`, `"host:8000"`); the engine's
    /// `toParsed` normalises both forms into u16 container ports.
    #[serde(default)]
    pub forward_ports: Vec<u16>,
    /// `devcontainer.json` `portsAttributes`, keyed by port number as a
    /// string — e.g. `{"9119": {"label": "Dashboard", "protocol": "http"}}`.
    ///
    /// Carried through verbatim, like `customizations`: the spec fixes a
    /// small set of keys, but consumers read at most `label` and `protocol`,
    /// and passing the object through means a key we do not model yet is not
    /// silently dropped.
    #[serde(default)]
    pub ports_attributes: Option<serde_json::Value>,
    #[serde(default)]
    pub remote_user: Option<String>,
    #[serde(default)]
    pub container_env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub remote_env: std::collections::HashMap<String, Option<String>>,
    /// Tool-specific configuration (e.g. `customizations.vscode`,
    /// `customizations.wiki3`). Carried through verbatim so embedding
    /// apps can read their own keys without a second pass over the
    /// raw file.
    #[serde(default)]
    pub customizations: Option<serde_json::Value>,
    #[serde(default)]
    pub on_create_command: Option<LifecycleCommand>,
    #[serde(default)]
    pub update_content_command: Option<LifecycleCommand>,
    #[serde(default)]
    pub post_create_command: Option<LifecycleCommand>,
    #[serde(default)]
    pub post_start_command: Option<LifecycleCommand>,
    #[serde(default)]
    pub post_attach_command: Option<LifecycleCommand>,
}

/// Error raised while translating a parsed `devcontainer.json` into a
/// runtime-agnostic [`ContainerSpec`].
///
/// These are *config* problems rather than runtime problems: every
/// backend would fail the same way, so they are surfaced before any
/// runtime is asked to do work.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// A `mounts` entry could not be interpreted. The original entry is
    /// carried verbatim so the user sees exactly what was refused.
    #[error("unsupported `mounts` entry {raw:?}: {reason}")]
    Mount { raw: String, reason: String },
}

/// Parse one `devcontainer.json` `mounts` entry into a [`MountSpec`].
///
/// Two syntaxes are accepted, matching what the upstream implementations
/// hand to the container engine:
///
/// * key/value — `source=hermes-opt-data,target=/opt/data,type=volume`
///   (the `docker run --mount` form, and what the engine bundle emits
///   for the object syntax)
/// * short — `./cache:/cache:ro` (the `docker run -v` form)
///
/// `readonly` / `ro` is honoured in both. Unknown key/value fields are
/// rejected rather than ignored: silently discarding a `consistency` or
/// `bind-propagation` request would produce a container that starts but
/// is not the one the user asked for.
pub fn parse_mount(raw: &str) -> Result<MountSpec, String> {
    let entry = raw.trim();
    if entry.is_empty() {
        return Err("entry is empty".to_string());
    }
    if entry.contains('=') {
        parse_kv_mount(entry)
    } else {
        parse_short_mount(entry)
    }
}

fn parse_kv_mount(entry: &str) -> Result<MountSpec, String> {
    let mut kind = None;
    let mut source: Option<String> = None;
    let mut target: Option<String> = None;
    let mut read_only = false;

    for field in entry.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("field {field:?} is not a `key=value` pair"));
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "type" => {
                kind = Some(match value.to_ascii_lowercase().as_str() {
                    "bind" => MountKind::Bind,
                    "volume" => MountKind::Volume,
                    other => {
                        return Err(format!(
                            "unsupported mount `type={other}` (expected `bind` or `volume`)"
                        ))
                    }
                });
            }
            "source" | "src" => source = Some(value.to_string()),
            "target" | "destination" | "dst" => target = Some(value.to_string()),
            "readonly" | "read-only" => {
                read_only = !matches!(value.to_ascii_lowercase().as_str(), "false" | "0" | "");
            }
            other => return Err(format!("unsupported mount field {other:?}")),
        }
    }

    let kind = kind.ok_or_else(|| "missing `type` (expected `bind` or `volume`)".to_string())?;
    finish_mount(kind, source, target, read_only)
}

fn parse_short_mount(entry: &str) -> Result<MountSpec, String> {
    let parts = split_short_mount(entry);
    if parts.len() < 2 || parts.len() > 3 {
        return Err("expected `source:target` or `source:target:options`".to_string());
    }
    let source = parts[0].trim().to_string();
    if source.is_empty() {
        return Err("missing `source`".to_string());
    }
    // The short form does not state bind vs volume; like the Docker and
    // Podman CLIs we infer it — a path is a bind mount, anything else is
    // a named volume. A Windows drive-letter source (`C:\src`) counts as
    // a path even though it does not start with `/`, `.` or `~`.
    let drive_letter = source.len() > 2
        && source.as_bytes()[1] == b':'
        && source
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
    let path_like = source.starts_with('/')
        || source.starts_with('.')
        || source.starts_with('~')
        || drive_letter;
    let kind = if path_like {
        MountKind::Bind
    } else {
        MountKind::Volume
    };
    let read_only = parts
        .get(2)
        .map(|opts| opts.split(',').any(|o| o.trim().eq_ignore_ascii_case("ro")))
        .unwrap_or(false);
    finish_mount(
        kind,
        Some(source),
        Some(parts[1].trim().to_string()),
        read_only,
    )
}

/// Shared tail of both mount parsers: apply defaults and reject the
/// combinations we cannot represent faithfully.
fn finish_mount(
    kind: MountKind,
    source: Option<String>,
    target: Option<String>,
    read_only: bool,
) -> Result<MountSpec, String> {
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        return Err("missing `target`".to_string());
    };
    let Some(source) = source.filter(|s| !s.is_empty()) else {
        // Anonymous volumes (`type=volume,target=/data` with no source)
        // are valid Docker but have no representation in
        // `MountSpec::source`. Reject explicitly rather than inventing a
        // name the user did not ask for.
        return Err("missing `source` (anonymous volumes are not supported)".to_string());
    };
    Ok(MountSpec {
        kind,
        source: PathBuf::from(source),
        target: PathBuf::from(target),
        read_only,
    })
}

/// Split the short `-v` form on `:` while leaving a leading Windows
/// drive letter (`C:\src`) intact.
fn split_short_mount(entry: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = entry.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' {
            // A single leading ASCII letter followed by a path separator
            // is a drive letter, not a field separator.
            let drive_letter = current.len() == 1
                && current.chars().all(|d| d.is_ascii_alphabetic())
                && chars.peek().is_some_and(|n| *n == '\\' || *n == '/');
            if !drive_letter {
                parts.push(std::mem::take(&mut current));
                continue;
            }
        }
        current.push(c);
    }
    parts.push(current);
    parts
}

/// Translate the parsed config into a runtime-agnostic [`ContainerSpec`],
/// given the already-resolved [`ImageRef`] (the lifecycle decides whether
/// it came from `pull` or `build`).
///
/// Returns [`TranslateError`] when the config asks for something we
/// cannot faithfully express — currently only an unparseable `mounts`
/// entry. Callers are expected to abort the launch rather than create a
/// container that quietly differs from the requested configuration.
pub fn to_container_spec(
    parsed: &ParsedDevContainer,
    image_ref: ImageRef,
    workspace_id: &str,
    host_workspace: &std::path::Path,
) -> Result<ContainerSpec, TranslateError> {
    let workspace_target = parsed.workspace_folder.clone().unwrap_or_else(|| {
        PathBuf::from("/workspaces").join(
            host_workspace
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(workspace_id),
        )
    });

    // The workspace bind mount is always first so the orchestrator's
    // `host_mounts` bookkeeping can rely on its position; project mounts
    // follow in declaration order.
    let mut mounts = vec![MountSpec {
        kind: MountKind::Bind,
        source: host_workspace.to_path_buf(),
        target: workspace_target.clone(),
        read_only: false,
    }];
    for raw in &parsed.mounts {
        mounts.push(parse_mount(raw).map_err(|reason| TranslateError::Mount {
            raw: raw.clone(),
            reason,
        })?);
    }

    let ports = parsed
        .forward_ports
        .iter()
        .map(|&p| crate::container::PortForward {
            host_port: p,
            container_port: p,
            protocol: crate::container::PortProtocol::Tcp,
        })
        .collect();

    let env = parsed
        .container_env
        .iter()
        .chain(
            parsed
                .remote_env
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|vv| (k, vv))),
        )
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let raw_name = derive_name_from_path(host_workspace, workspace_id);
    let name =
        sanitize_entity_name(&raw_name).unwrap_or_else(|| format!("devcontainer-{workspace_id}"));

    // Devcontainers spec defaults `overrideCommand` to true, and the
    // sleep loop below is what that default means: it replaces the
    // image's CMD/ENTRYPOINT with a long-running no-op so the container
    // stays alive for `exec`-driven workflows. Without it, base images
    // whose CMD exits immediately (e.g. `bash` without a TTY) leave us
    // with a "stopped" container the moment `start` returns and `exec`
    // then fails with "no sandbox client exists: container is stopped".
    //
    // When the project opts out with `overrideCommand: false` we must
    // substitute nothing at all: `None` leaves the image's own
    // ENTRYPOINT/CMD in charge, which long-running service images
    // (s6-supervised agents, `CMD ["gateway", "run"]`, …) depend on.
    let command = if parsed.override_command == Some(false) {
        None
    } else {
        Some(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "while sleep 2147483647; do :; done".to_string(),
        ])
    };

    Ok(ContainerSpec {
        name,
        image: image_ref,
        command,
        workdir: Some(workspace_target),
        env,
        mounts,
        ports,
        user: parsed.remote_user.clone(),
        privileged: false,
        run_args: parsed.run_args.clone(),
        labels: std::collections::HashMap::new(),
    })
}

/// Derive a deterministic, repo-unique container name from the host
/// workspace path. Format is `<basename>-<8-hex>` where the hex is a
/// stable hash of the full absolute path. This guarantees:
///
/// * Two repos called `take-two` in different parent directories don't
///   collide on the runtime's name index (the hash differs).
/// * Re-running `up` on the same repo produces the same name, so the
///   orchestrator's adopt-on-already-exists path can find it again.
///
/// The devcontainer.json `name` is intentionally ignored: it is
/// free-form display text shared by every fork of a template.
pub(crate) fn derive_name_from_path(
    host_workspace: &std::path::Path,
    workspace_id: &str,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let basename = host_workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("devcontainer");
    let mut h = DefaultHasher::new();
    host_workspace.hash(&mut h);
    // Mix the workspace id in too so two different host registrations of
    // the same path get distinct containers (rare but possible).
    workspace_id.hash(&mut h);
    let suffix = format!("{:08x}", (h.finish() as u32));
    format!("{basename}-{suffix}")
}

/// Coerce an arbitrary devcontainer `name` (which is free-form, e.g.
/// `"JupyterLite Demo"`) into a string that container runtimes will
/// accept as an entity name. Apple's `container` CLI in particular
/// rejects spaces and many punctuation characters with `invalid entity
/// name`. We keep ASCII alphanumerics, dash and underscore; everything
/// else collapses to a single dash. Returns `None` if nothing usable
/// remains so the caller can fall back to a workspace-id based name.
pub(crate) fn sanitize_entity_name(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    let mut last_was_dash = false;
    for c in input.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if keep {
            out.push(c);
            last_was_dash = c == '-';
        } else if !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    // Container CLIs typically require the first character to be alpha-
    // numeric; if we somehow ended up leading with `_` or `-`, drop them.
    while out
        .chars()
        .next()
        .is_some_and(|c| !c.is_ascii_alphanumeric())
    {
        out.remove(0);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn parse_image_ref(s: &str) -> ImageRef {
    // Minimal parser: registry/repo:tag@digest. Sufficient for v1.
    let (rest, digest) = match s.split_once('@') {
        Some((r, d)) => (r, Some(d.to_string())),
        None => (s, None),
    };
    let (rest, tag) = match rest.rsplit_once(':') {
        // Avoid mistaking a port in the registry for a tag.
        Some((r, t)) if !t.contains('/') => (r, Some(t.to_string())),
        _ => (rest, None),
    };
    let (registry, repository) = match rest.split_once('/') {
        Some((r, repo)) if r.contains('.') || r.contains(':') => {
            (Some(r.to_string()), repo.to_string())
        }
        _ => (None, rest.to_string()),
    };
    ImageRef {
        registry,
        repository,
        tag,
        digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_repository() {
        let r = parse_image_ref("ubuntu");
        assert_eq!(r.repository, "ubuntu");
        assert_eq!(r.tag, None);
        assert_eq!(r.registry, None);
    }

    #[test]
    fn parses_repo_with_tag() {
        let r = parse_image_ref("node:20-alpine");
        assert_eq!(r.repository, "node");
        assert_eq!(r.tag.as_deref(), Some("20-alpine"));
    }

    #[test]
    fn sanitize_entity_name_replaces_spaces_and_punctuation() {
        assert_eq!(
            sanitize_entity_name("JupyterLite Demo").as_deref(),
            Some("JupyterLite-Demo")
        );
        assert_eq!(
            sanitize_entity_name("  hello / world!  ").as_deref(),
            Some("hello-world")
        );
        assert_eq!(
            sanitize_entity_name("already_ok-1").as_deref(),
            Some("already_ok-1")
        );
        assert_eq!(sanitize_entity_name("   "), None);
        assert_eq!(sanitize_entity_name(""), None);
    }

    #[test]
    fn to_container_spec_derives_name_from_repo_path() {
        // Free-form devcontainer.json `name` is intentionally ignored:
        // it's display text shared by every fork of a template, so two
        // repos with the same `name` would collide on the runtime's
        // global container-name index. The container name is derived
        // from the host repo path instead.
        let parsed = ParsedDevContainer {
            name: Some("JupyterLite Demo".into()),
            image: Some("ubuntu:24.04".into()),
            ..Default::default()
        };
        let spec_a = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws-1",
            std::path::Path::new("/tmp/take-two"),
        )
        .expect("translate");
        let spec_b = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws-2",
            std::path::Path::new("/tmp/new-from-temp"),
        )
        .expect("translate");
        // Two different repos with the same devcontainer name must
        // produce distinct container names.
        assert_ne!(spec_a.name, spec_b.name);
        assert!(spec_a.name.starts_with("take-two-"), "got {}", spec_a.name);
        assert!(
            spec_b.name.starts_with("new-from-temp-"),
            "got {}",
            spec_b.name
        );
        // Same path + workspace_id is deterministic so adoption works.
        let again = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws-1",
            std::path::Path::new("/tmp/take-two"),
        )
        .expect("translate");
        assert_eq!(spec_a.name, again.name);
    }

    #[test]
    fn to_container_spec_forwards_run_args_verbatim() {
        let parsed = ParsedDevContainer {
            image: Some("ubuntu:24.04".into()),
            run_args: vec!["--cpus=4".into(), "--memory=8g".into()],
            ..Default::default()
        };
        let spec = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws-1",
            std::path::Path::new("/tmp/repo"),
        )
        .expect("translate");
        assert_eq!(spec.run_args, vec!["--cpus=4", "--memory=8g"]);
    }

    #[test]
    fn parses_registry_repo_tag_digest() {
        let r = parse_image_ref("ghcr.io/example/app:1.0@sha256:deadbeef");
        assert_eq!(r.registry.as_deref(), Some("ghcr.io"));
        assert_eq!(r.repository, "example/app");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
        assert_eq!(r.digest.as_deref(), Some("sha256:deadbeef"));
    }

    // -----------------------------------------------------------------
    // Hermes acceptance fixture.
    //
    // `wiki3-ai/hermes-devcontainer` is the real-world configuration the
    // common layer has to get right: a custom Dockerfile over a
    // long-running service image, `overrideCommand:false` so the image's
    // `CMD ["gateway", "run"]` survives, a named volume for durable
    // state, host aliases via `runArgs`, and forwarded ports. These are
    // generic Dev Container behaviours — nothing here is special-cased
    // for Hermes.
    // -----------------------------------------------------------------

    #[test]
    fn ports_attributes_round_trip_as_camel_case() {
        // The field travels WebView → Rust as JSON, so the wire name has to be
        // `portsAttributes`. Getting that wrong is silent: the value arrives as
        // `None` and the port panel just loses its labels.
        let parsed = ParsedDevContainer {
            forward_ports: vec![9119],
            ports_attributes: Some(serde_json::json!({
                "9119": { "label": "Dashboard", "protocol": "http" }
            })),
            ..Default::default()
        };

        let json = serde_json::to_value(&parsed).unwrap();
        assert!(
            json.get("portsAttributes").is_some(),
            "wrong wire name in {json}"
        );
        assert_eq!(json["portsAttributes"]["9119"]["label"], "Dashboard");

        let back: ParsedDevContainer = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.ports_attributes.as_ref().unwrap()["9119"]["protocol"],
            "http"
        );
    }

    #[test]
    fn ports_attributes_default_to_none_when_absent() {
        // A config that carries no `portsAttributes` must stay loadable.
        let parsed: ParsedDevContainer =
            serde_json::from_value(serde_json::json!({ "image": "alpine" })).unwrap();
        assert!(parsed.ports_attributes.is_none());
    }

    fn hermes_parsed() -> ParsedDevContainer {
        ParsedDevContainer {
            name: Some("Hermes Agent + Unsloth".into()),
            build: Some(DevContainerBuild {
                dockerfile: Some("Dockerfile".into()),
                context: Some(".".into()),
                ..Default::default()
            }),
            override_command: Some(false),
            mounts: vec!["source=hermes-opt-data,target=/opt/data,type=volume".into()],
            run_args: vec![
                "--add-host=host.docker.internal:host-gateway".into(),
                "--add-host=host.containers.internal:host-gateway".into(),
            ],
            forward_ports: vec![8642, 9119],
            container_env: [
                ("HERMES_DASHBOARD".to_string(), "1".to_string()),
                ("HERMES_DASHBOARD_HOST".to_string(), "0.0.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            post_create_command: Some(LifecycleCommand::Single(
                ".devcontainer/configure-hermes.sh".into(),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn hermes_config_survives_translation() {
        let parsed = hermes_parsed();
        let spec = to_container_spec(
            &parsed,
            parse_image_ref("hermes-devcontainer:test"),
            "ws-hermes",
            std::path::Path::new("/tmp/hermes-devcontainer"),
        )
        .expect("translate");

        // `overrideCommand:false` => the image keeps its own CMD. This is
        // what keeps `gateway run` alive; injecting the keepalive sleep
        // loop here would start the container with the wrong process.
        assert_eq!(
            spec.command, None,
            "`overrideCommand:false` must not substitute a command"
        );

        // Workspace bind mount first, then the project's named volume.
        assert_eq!(spec.mounts.len(), 2);
        assert_eq!(spec.mounts[0].kind, MountKind::Bind);
        assert_eq!(spec.mounts[1].kind, MountKind::Volume);
        assert_eq!(spec.mounts[1].source, PathBuf::from("hermes-opt-data"));
        assert_eq!(spec.mounts[1].target, PathBuf::from("/opt/data"));
        assert!(!spec.mounts[1].read_only);

        // runArgs reach the runtime untouched: whether they are
        // *supported* is the runtime's call, not the translator's.
        assert_eq!(spec.run_args.len(), 2);
        assert!(spec.run_args[0].starts_with("--add-host=host.docker.internal"));

        assert_eq!(
            spec.env.get("HERMES_DASHBOARD").map(String::as_str),
            Some("1")
        );
        let ports: Vec<u16> = spec.ports.iter().map(|p| p.container_port).collect();
        assert_eq!(ports, vec![8642, 9119]);
    }

    #[test]
    fn override_command_absent_or_true_installs_keepalive() {
        for override_command in [None, Some(true)] {
            let parsed = ParsedDevContainer {
                image: Some("ubuntu:24.04".into()),
                override_command,
                ..Default::default()
            };
            let spec = to_container_spec(
                &parsed,
                parse_image_ref("ubuntu:24.04"),
                "ws",
                std::path::Path::new("/tmp/repo"),
            )
            .expect("translate");
            let cmd = spec
                .command
                .unwrap_or_else(|| panic!("expected keepalive for {override_command:?}"));
            assert_eq!(cmd[0], "/bin/sh");
            assert!(
                cmd[2].contains("sleep"),
                "expected sleep loop for {override_command:?}, got {cmd:?}"
            );
        }
    }

    #[test]
    fn parse_mount_reads_the_docker_mount_form() {
        let m = parse_mount("source=hermes-opt-data,target=/opt/data,type=volume").expect("parse");
        assert_eq!(m.kind, MountKind::Volume);
        assert_eq!(m.source, PathBuf::from("hermes-opt-data"));
        assert_eq!(m.target, PathBuf::from("/opt/data"));
        assert!(!m.read_only);
    }

    #[test]
    fn parse_mount_reads_the_short_form_and_infers_kind() {
        let bind = parse_mount("/host/cache:/cache:ro").expect("parse");
        assert_eq!(bind.kind, MountKind::Bind);
        assert!(bind.read_only);

        let vol = parse_mount("cache-vol:/cache").expect("parse");
        assert_eq!(vol.kind, MountKind::Volume);
        assert!(!vol.read_only);
    }

    #[test]
    fn parse_mount_keeps_windows_drive_letters_intact() {
        let m = parse_mount(r"C:\Users\me\src:/src").expect("parse");
        // The drive-letter colon must not be treated as a separator, and
        // a drive path is a bind mount even though it lacks a leading `/`.
        assert_eq!(m.kind, MountKind::Bind);
        assert_eq!(m.source, PathBuf::from(r"C:\Users\me\src"));
        assert_eq!(m.target, PathBuf::from("/src"));
    }

    #[test]
    fn parse_mount_rejects_what_it_cannot_express() {
        // Unknown fields are refused rather than dropped — silently
        // ignoring `consistency` would start a container that is not the
        // one the user asked for.
        assert!(parse_mount("source=a,target=/b,type=bind,consistency=cached").is_err());
        assert!(parse_mount("source=a,target=/b,type=tmpfs").is_err());
        // Anonymous volumes have no representation in `MountSpec::source`.
        assert!(parse_mount("target=/data,type=volume").is_err());
        assert!(parse_mount("").is_err());
    }

    #[test]
    fn unparseable_mount_fails_translation_instead_of_being_dropped() {
        let parsed = ParsedDevContainer {
            image: Some("ubuntu:24.04".into()),
            mounts: vec!["source=a,target=/b,type=tmpfs".into()],
            ..Default::default()
        };
        let err = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws",
            std::path::Path::new("/tmp/repo"),
        )
        .expect_err("an unsupported mount type must not be silently dropped");
        let msg = err.to_string();
        assert!(msg.contains("tmpfs"), "error should quote the entry: {msg}");
    }
}
