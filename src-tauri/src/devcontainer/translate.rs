//! Translate a parsed `devcontainer.json` (received from the WebView spec
//! engine) into a [`ContainerSpec`] suitable for any
//! [`crate::container::ContainerRuntime`]. Only the v1 subset of fields is
//! consumed; later phases enrich it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::container::{ContainerSpec, ImageRef};

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
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Verbatim docker-style flags from devcontainer.json `runArgs`.
    /// Forwarded as-is to the runtime's `create` invocation so users
    /// can request resource limits (`--cpus=4`, `--memory=8g`),
    /// extra capabilities, etc. Per the upstream spec these are
    /// passed to `docker run` exactly as written.
    #[serde(default)]
    pub run_args: Vec<String>,
    #[serde(default)]
    pub forward_ports: Vec<u16>,
    #[serde(default)]
    pub remote_user: Option<String>,
    #[serde(default)]
    pub container_env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub remote_env: std::collections::HashMap<String, Option<String>>,
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

/// Translate the parsed config into a runtime-agnostic [`ContainerSpec`],
/// given the already-resolved [`ImageRef`] (the lifecycle decides whether
/// it came from `pull` or `build`).
pub fn to_container_spec(
    parsed: &ParsedDevContainer,
    image_ref: ImageRef,
    workspace_id: &str,
    host_workspace: &std::path::Path,
) -> ContainerSpec {
    let workspace_target = parsed.workspace_folder.clone().unwrap_or_else(|| {
        PathBuf::from("/workspaces").join(
            host_workspace
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(workspace_id),
        )
    });

    let mounts = vec![crate::container::MountSpec {
        kind: crate::container::MountKind::Bind,
        source: host_workspace.to_path_buf(),
        target: workspace_target.clone(),
        read_only: false,
    }];

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

    ContainerSpec {
        name,
        image: image_ref,
        // Devcontainers spec defaults `overrideCommand` to true: replace
        // the image's CMD/ENTRYPOINT with a long-running no-op so the
        // container stays alive for `exec`-driven workflows. Without
        // this, base images whose CMD exits immediately (e.g. `bash`
        // without a TTY) leave us with a "stopped" container the moment
        // `start` returns and `exec` then fails with "no sandbox
        // client exists: container is stopped".
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "while sleep 2147483647; do :; done".into(),
        ]),
        workdir: Some(workspace_target),
        env,
        mounts,
        ports,
        user: parsed.remote_user.clone(),
        privileged: false,
        run_args: parsed.run_args.clone(),
    }
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
        );
        let spec_b = to_container_spec(
            &parsed,
            parse_image_ref("ubuntu:24.04"),
            "ws-2",
            std::path::Path::new("/tmp/new-from-temp"),
        );
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
        );
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
        );
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
}
