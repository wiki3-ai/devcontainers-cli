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

/// Subset of the parsed `devcontainer.json` fields needed by the v1 MVP.
/// This is the contract sent from the WebView to the Rust host. Fields
/// outside this struct (Features, compose, etc.) are deferred to later
/// phases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedDevContainer {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// Raw `build` object (Dockerfile-based config). Carried through so
    /// the host can detect-and-reject; the v1 lifecycle does not yet
    /// build images.
    #[serde(default)]
    pub build: Option<serde_json::Value>,
    /// Raw `dockerComposeFile` value (string or array). Carried through
    /// for the same reason as `build`.
    #[serde(default, rename = "dockerComposeFile")]
    pub docker_compose_file: Option<serde_json::Value>,
    #[serde(default)]
    pub workspace_folder: Option<PathBuf>,
    #[serde(default)]
    pub workspace_mount: Option<String>,
    #[serde(default)]
    pub mounts: Vec<String>,
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

/// Translate the parsed config into a runtime-agnostic [`ContainerSpec`].
/// This is the minimal v1 implementation; later phases enrich it.
pub fn to_container_spec(
    parsed: &ParsedDevContainer,
    workspace_id: &str,
    host_workspace: &std::path::Path,
) -> ContainerSpec {
    let image_ref = parsed
        .image
        .as_deref()
        .map(parse_image_ref)
        .unwrap_or_else(|| {
            // Caller is expected to validate via the lifecycle layer;
            // this fallback only kicks in for old/incomplete callers
            // (e.g. tests) and is intentionally obvious so it surfaces
            // in logs rather than silently running a stand-in image.
            ImageRef {
                registry: None,
                repository: "devcontainer-image-unspecified".to_string(),
                tag: None,
                digest: None,
            }
        });

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

    let raw_name = parsed
        .name
        .clone()
        .unwrap_or_else(|| format!("devcontainer-{workspace_id}"));
    let name = sanitize_entity_name(&raw_name)
        .unwrap_or_else(|| format!("devcontainer-{workspace_id}"));

    ContainerSpec {
        name,
        image: image_ref,
        command: None,
        workdir: Some(workspace_target),
        env,
        mounts,
        ports,
        user: parsed.remote_user.clone(),
        privileged: false,
    }
}

/// Coerce an arbitrary devcontainer `name` (which is free-form, e.g.
/// `"JupyterLite Demo"`) into a string that container runtimes will
/// accept as an entity name. Apple's `container` CLI in particular
/// rejects spaces and many punctuation characters with `invalid entity
/// name`. We keep ASCII alphanumerics, dash and underscore; everything
/// else collapses to a single dash. Returns `None` if nothing usable
/// remains so the caller can fall back to a workspace-id based name.
fn sanitize_entity_name(input: &str) -> Option<String> {
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
    if out.is_empty() { None } else { Some(out) }
}

fn parse_image_ref(s: &str) -> ImageRef {
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
    fn to_container_spec_sanitizes_devcontainer_name() {
        let parsed = ParsedDevContainer {
            name: Some("JupyterLite Demo".into()),
            image: Some("ubuntu:24.04".into()),
            ..Default::default()
        };
        let spec = to_container_spec(&parsed, "ws-1", std::path::Path::new("/tmp/take-two"));
        assert_eq!(spec.name, "JupyterLite-Demo");
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
