//! Translate a parsed `devcontainer.json` (received from the WebView spec
//! engine) into a [`ContainerSpec`] suitable for any
//! [`crate::container::ContainerRuntime`]. Implementation lands in step 7 of
//! the conversion roadmap.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::container::{ContainerSpec, ImageRef};

/// Subset of the parsed `devcontainer.json` fields needed by the v1 MVP.
/// This is the contract sent from the WebView to the Rust host. Fields
/// outside this struct (Features, compose, etc.) are deferred to later
/// phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDevContainer {
    pub name: Option<String>,
    pub image: Option<String>,
    pub workspace_folder: Option<PathBuf>,
    pub workspace_mount: Option<String>,
    pub mounts: Vec<String>,
    pub forward_ports: Vec<u16>,
    pub remote_user: Option<String>,
    pub container_env: std::collections::HashMap<String, String>,
    pub remote_env: std::collections::HashMap<String, Option<String>>,
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
        .unwrap_or_else(|| ImageRef {
            registry: None,
            repository: "mcr.microsoft.com/devcontainers/base".to_string(),
            tag: Some("ubuntu".to_string()),
            digest: None,
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

    ContainerSpec {
        name: parsed
            .name
            .clone()
            .unwrap_or_else(|| format!("devcontainer-{workspace_id}")),
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
    fn parses_registry_repo_tag_digest() {
        let r = parse_image_ref("ghcr.io/example/app:1.0@sha256:deadbeef");
        assert_eq!(r.registry.as_deref(), Some("ghcr.io"));
        assert_eq!(r.repository, "example/app");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
        assert_eq!(r.digest.as_deref(), Some("sha256:deadbeef"));
    }
}
