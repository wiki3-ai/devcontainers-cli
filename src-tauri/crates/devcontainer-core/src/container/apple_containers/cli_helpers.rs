//! Standalone CLI helpers for Apple Container, complementing the
//! struct-based [`super::AppleContainersRuntime`].
//!
//! These are the bits that are useful to embedding apps before (or
//! independently of) instantiating a [`crate::container::ContainerRuntime`]:
//!
//! * detection — find the `container` binary on disk;
//! * service lifecycle — start/stop the macOS-wide system service;
//! * by-name container ops — sibling apps (e.g. Wiki3) name their
//!   ephemeral `--rm` containers themselves and need to operate on
//!   them by name rather than by the runtime's internal id.
//!
//! The runtime backend in `super::mod` has its own copies of some of
//! these (e.g. `ensure_system_running` cached behind an AtomicBool).
//! These helpers are deliberately uncached and shell out fresh on
//! every call so callers retain control over re-prompting / retry
//! semantics.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Standard locations to probe before falling back to `PATH`. Order
/// matters only for reporting — any hit is treated as equivalent.
const STANDARD_PATHS: &[&str] = &["/usr/local/bin/container", "/opt/homebrew/bin/container"];

/// Result of probing for Apple Container on this system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppleContainerStatus {
    /// Whether a `container` binary was found.
    pub installed: bool,
    /// Absolute path of the resolved binary, if any.
    pub path: Option<PathBuf>,
}

/// Probe for Apple Container using a caller-supplied directory list.
/// Exposed so tests can exercise the logic without touching `/usr/…`.
pub fn probe_with_dirs(standard_paths: &[&Path], path_env: Option<&str>) -> AppleContainerStatus {
    for p in standard_paths {
        if is_runnable(p) {
            return AppleContainerStatus {
                installed: true,
                path: Some(p.to_path_buf()),
            };
        }
    }
    if let Some(path_env) = path_env {
        for dir in split_path_env(path_env) {
            let candidate = dir.join("container");
            if is_runnable(&candidate) {
                return AppleContainerStatus {
                    installed: true,
                    path: Some(candidate),
                };
            }
        }
    }
    AppleContainerStatus {
        installed: false,
        path: None,
    }
}

/// Detect Apple Container on the current system.
pub fn detect() -> AppleContainerStatus {
    let standard: Vec<PathBuf> = STANDARD_PATHS.iter().map(PathBuf::from).collect();
    let standard_refs: Vec<&Path> = standard.iter().map(|p| p.as_path()).collect();
    let path_env = std::env::var("PATH").ok();
    probe_with_dirs(&standard_refs, path_env.as_deref())
}

/// Ensure the Apple Container system service (which owns the UNIX
/// socket at `~/Library/Containers/com.apple.container/Data/container.sock`)
/// is running. Runs `container system start` which is idempotent —
/// it's a no-op if the service is already up.
///
/// On first start Apple Container prompts on stdin to download its
/// default Kata kernel. We auto-accept by feeding `y\n`; the
/// alternative (failing with "failed to read user input") leaves the
/// service half-initialized and unusable.
///
/// On first start Apple may also show a system authorization prompt
/// (sometimes several seconds), and the kernel download itself can
/// take a while, so we impose a generous timeout.
pub async fn ensure_service_running(container_bin: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut cmd = Command::new(container_bin);
    cmd.arg("system").arg("start");

    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `container system start`: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"y\n").await;
        drop(stdin);
    }

    let output = tokio::time::timeout(Duration::from_secs(600), child.wait_with_output())
        .await
        .map_err(|_| "`container system start` timed out after 10 minutes".to_string())?
        .map_err(|e| format!("wait for `container system start`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "`container system start` failed (exit {:?}):\n--- stderr ---\n{}\n--- stdout ---\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        ));
    }
    Ok(())
}

/// Probe whether the Apple Container service is currently running.
/// Runs `container ls -q` with a short timeout; the command fails
/// fast when the service socket isn't up. Returns `true` only when
/// the service is responsive.
pub async fn is_service_running(container_bin: &Path) -> bool {
    use tokio::process::Command;

    let fut = Command::new(container_bin).arg("ls").arg("-q").output();
    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(out)) => out.status.success(),
        _ => false,
    }
}

/// Stop the Apple Container system service (best-effort). Used on
/// app quit when the embedding app started the service itself.
pub async fn stop_service(container_bin: &Path) -> Result<(), String> {
    use tokio::process::Command;

    let fut = Command::new(container_bin)
        .arg("system")
        .arg("stop")
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .map_err(|_| "`container system stop` timed out".to_string())?
        .map_err(|e| format!("spawn `container system stop`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`container system stop` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// List names of currently running containers (best-effort). Returns
/// an empty vec if the service is down or the command fails.
///
/// Differs from [`crate::container::ContainerRuntime::list_containers`]
/// by returning bare names: embedding apps that name their own
/// `--rm` containers (e.g. `wiki3-site-<tag>`) want the names back,
/// not the runtime's internal status structs.
pub async fn list_running_container_names(container_bin: &Path) -> Vec<String> {
    use tokio::process::Command;

    let fut = Command::new(container_bin)
        .arg("ls")
        .arg("--format")
        .arg("json")
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|obj| {
            obj.get("name")
                .or_else(|| obj.get("Name"))
                .or_else(|| obj.get("configuration").and_then(|c| c.get("id")))
                .or_else(|| obj.get("id"))
                .or_else(|| obj.get("ID"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Stop a specific container by name. Returns `Ok(())` even if the
/// container is already gone (treats "not found" as success).
pub async fn stop_container_by_name(container_bin: &Path, name: &str) -> Result<(), String> {
    use tokio::process::Command;

    let fut = Command::new(container_bin).arg("stop").arg(name).output();
    let out = tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .map_err(|_| format!("`container stop {name}` timed out"))?
        .map_err(|e| format!("spawn `container stop`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        let lc = err.to_lowercase();
        if lc.contains("not found") || lc.contains("no such") {
            return Ok(());
        }
        return Err(format!("`container stop {name}` failed: {err}"));
    }
    Ok(())
}

fn split_path_env(path_env: &str) -> Vec<PathBuf> {
    path_env
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn is_runnable(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(md) => {
            if !md.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                md.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn make_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn reports_not_installed_when_nothing_found() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does-not-exist");
        let status = probe_with_dirs(&[bogus.as_path()], Some(""));
        assert_eq!(
            status,
            AppleContainerStatus {
                installed: false,
                path: None
            }
        );
    }

    #[test]
    fn finds_binary_in_standard_path() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("container");
        fs::write(&exe, b"#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        make_executable(&exe);

        let status = probe_with_dirs(&[exe.as_path()], None);
        assert!(status.installed);
        assert_eq!(status.path.as_deref(), Some(exe.as_path()));
    }

    #[test]
    fn falls_back_to_path_env() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("container");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        make_executable(&exe);

        let path_env = format!("/nowhere:{}", bin_dir.display());
        let status = probe_with_dirs(&[], Some(&path_env));
        assert!(status.installed);
        assert_eq!(status.path.as_deref(), Some(exe.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_not_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("container");
        fs::write(&exe, b"not actually a binary").unwrap();
        let status = probe_with_dirs(&[exe.as_path()], None);
        assert!(!status.installed);
    }

    #[test]
    fn directory_named_container_is_not_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("container");
        fs::create_dir(&path).unwrap();
        let status = probe_with_dirs(&[path.as_path()], None);
        assert!(!status.installed);
    }

    #[test]
    fn empty_path_env_is_safe() {
        let status = probe_with_dirs(&[], Some(""));
        assert!(!status.installed);
    }

    #[test]
    fn standard_path_wins_over_path_env() {
        let tmp = tempfile::tempdir().unwrap();
        let standard = tmp.path().join("container");
        fs::write(&standard, b"").unwrap();
        #[cfg(unix)]
        make_executable(&standard);

        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let path_exe = bin_dir.join("container");
        fs::write(&path_exe, b"").unwrap();
        #[cfg(unix)]
        make_executable(&path_exe);

        let path_env = bin_dir.display().to_string();
        let status = probe_with_dirs(&[standard.as_path()], Some(&path_env));
        assert_eq!(status.path.as_deref(), Some(standard.as_path()));
    }
}
