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
/// On first start Apple Container needs to install its default Kata
/// kernel; without an answer it prompts on the controlling TTY and
/// fails with "failed to read user input" when launched from a GUI
/// app (no TTY attached). We pass `--enable-kernel-install` to
/// non-interactively accept the install — the alternative leaves the
/// service half-initialized and subsequent `container build` calls
/// fail with "default kernel not configured for architecture arm64".
///
/// The kernel download plus a possible system authorization prompt
/// (sometimes several seconds) can take a while, so we impose a
/// generous timeout.
pub async fn ensure_service_running(container_bin: &Path) -> Result<(), String> {
    ensure_service_running_with_log(container_bin, None).await
}

/// Variant of [`ensure_service_running`] that streams stdout/stderr
/// from the underlying `container` invocations line-by-line into the
/// supplied sink. Useful for surfacing the kernel-download progress
/// (which can take a couple of minutes on first run) in the embedding
/// app's UI rather than leaving the user staring at a spinner.
///
/// The sender is fed bare lines (no stream tag) — the caller is
/// responsible for tagging them as `system` / `stdout` / `stderr` if
/// it needs that distinction.
pub async fn ensure_service_running_with_log(
    container_bin: &Path,
    log_sink: Option<tokio::sync::mpsc::Sender<String>>,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    if let Some(sink) = &log_sink {
        let _ = sink
            .send(format!(
                "$ {} system start --enable-kernel-install",
                container_bin.display()
            ))
            .await;
    }

    let mut cmd = Command::new(container_bin);
    cmd.arg("system")
        .arg("start")
        .arg("--enable-kernel-install");

    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `container system start`: {e}"))?;

    // Stream stdout/stderr line-by-line into the sink while also
    // accumulating them into buffers so we can include them in the
    // error message on failure. This is what surfaces the kernel
    // download progress to the UI.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe missing".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe missing".to_string())?;
    let stdout_buf = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let stderr_buf = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let stdout_task = {
        let buf = stdout_buf.clone();
        let sink = log_sink.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut b = buf.lock();
                    b.push_str(&line);
                    b.push('\n');
                }
                if let Some(sink) = &sink {
                    let _ = sink.send(line).await;
                }
            }
        })
    };
    let stderr_task = {
        let buf = stderr_buf.clone();
        let sink = log_sink.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut b = buf.lock();
                    b.push_str(&line);
                    b.push('\n');
                }
                if let Some(sink) = &sink {
                    let _ = sink.send(line).await;
                }
            }
        })
    };

    let status = tokio::time::timeout(Duration::from_secs(600), child.wait())
        .await
        .map_err(|_| "`container system start` timed out after 10 minutes".to_string())?
        .map_err(|e| format!("wait for `container system start`: {e}"))?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    if !status.success() {
        return Err(format!(
            "`container system start` failed (exit {:?}):\n--- stderr ---\n{}\n--- stdout ---\n{}",
            status.code(),
            stderr_buf.lock(),
            stdout_buf.lock(),
        ));
    }

    // Belt-and-suspenders: if the user previously ran `container
    // system start` interactively and answered "no" (or the prompt
    // failed and the service started anyway with no kernel), the
    // service is up but builds will fail with "default kernel not
    // configured for architecture <arch>". `container system start`
    // is then a no-op — it won't retry the install. Detect that
    // state and run `container system kernel set --recommended` to
    // self-heal without bouncing out to the command line.
    if !default_kernel_installed() {
        if let Some(sink) = &log_sink {
            let _ = sink
                .send(
                    "default kernel not installed; downloading recommended kernel (this can take a couple of minutes on first run)…"
                        .to_string(),
                )
                .await;
        }
        ensure_default_kernel(container_bin, log_sink.as_ref()).await?;
        if let Some(sink) = &log_sink {
            let _ = sink.send("default kernel installed".to_string()).await;
        }
    }
    Ok(())
}

/// Detect whether Apple Container has a default kernel configured for
/// the current architecture. Apple Container stores kernels under
/// `~/Library/Application Support/com.apple.container/kernels/` keyed
/// by arch (`default.kernel-arm64` / `default.kernel-amd64`).
fn default_kernel_installed() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return true; // can't tell — don't second-guess
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        return true; // unknown arch — let the CLI decide
    };
    let path = PathBuf::from(home)
        .join("Library/Application Support/com.apple.container/kernels")
        .join(format!("default.kernel-{arch}"));
    path.exists()
}

/// Run `container system kernel set --recommended` to install Apple
/// Container's recommended default kernel non-interactively. Used to
/// recover from a half-initialised state where the service is up but
/// no default kernel is configured.
async fn ensure_default_kernel(
    container_bin: &Path,
    log_sink: Option<&tokio::sync::mpsc::Sender<String>>,
) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    if let Some(sink) = log_sink {
        let _ = sink
            .send(format!(
                "$ {} system kernel set --recommended",
                container_bin.display()
            ))
            .await;
    }

    let mut child = Command::new(container_bin)
        .arg("system")
        .arg("kernel")
        .arg("set")
        .arg("--recommended")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `container system kernel set`: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe missing".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe missing".to_string())?;
    let stdout_buf = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let stderr_buf = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let stdout_task = {
        let buf = stdout_buf.clone();
        let sink = log_sink.cloned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut b = buf.lock();
                    b.push_str(&line);
                    b.push('\n');
                }
                if let Some(sink) = &sink {
                    let _ = sink.send(line).await;
                }
            }
        })
    };
    let stderr_task = {
        let buf = stderr_buf.clone();
        let sink = log_sink.cloned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut b = buf.lock();
                    b.push_str(&line);
                    b.push('\n');
                }
                if let Some(sink) = &sink {
                    let _ = sink.send(line).await;
                }
            }
        })
    };

    let status = tokio::time::timeout(Duration::from_secs(600), child.wait())
        .await
        .map_err(|_| {
            "`container system kernel set --recommended` timed out after 10 minutes".to_string()
        })?
        .map_err(|e| format!("wait for `container system kernel set`: {e}"))?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    if !status.success() {
        return Err(format!(
            "`container system kernel set --recommended` failed (exit {:?}):\n--- stderr ---\n{}\n--- stdout ---\n{}",
            status.code(),
            stderr_buf.lock(),
            stdout_buf.lock(),
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

/// Look up the IPv4 address of a running container, by name. Returns
/// `None` if the container is not found, has no network, or `container
/// inspect` fails.
///
/// This is useful when the host's loopback publish-proxy is unhealthy
/// (e.g. corp-laptop network filters that RST `127.0.0.1:<hostPort>`):
/// the embedding app can race a probe against the direct vmnet
/// address as a fallback. The returned string is the bare IPv4
/// (`192.168.64.4`), with any CIDR suffix stripped.
pub async fn inspect_container_ipv4(container_bin: &Path, name: &str) -> Option<String> {
    use tokio::process::Command;

    let fut = Command::new(container_bin)
        .arg("inspect")
        .arg(name)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return None,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    // `container inspect <name>` returns an array with a single
    // object. Pick that object out, then walk `networks[0]
    // .ipv4Address`.
    let obj = match v {
        serde_json::Value::Array(a) => a.into_iter().next()?,
        v @ serde_json::Value::Object(_) => v,
        _ => return None,
    };
    let networks = obj.get("networks")?.as_array()?;
    let first = networks.first()?;
    let raw = first.get("ipv4Address")?.as_str()?;
    // Strip CIDR suffix if present (`192.168.64.4/24` -> `192.168.64.4`).
    let bare = raw.split('/').next()?.trim();
    if bare.is_empty() {
        return None;
    }
    Some(bare.to_string())
}

/// Find a running container whose mount-source matches `local_path`,
/// returning `(name, ipv4)` if any.
///
/// Apple Container's `container ls --format json` returns the full
/// per-container configuration including `mounts[].source` and
/// `networks[].ipv4Address`. Embedding apps that lose track of
/// their containers across restarts (e.g. Wiki3 keeps its
/// in-memory `LocalSiteManager` only for the running session) use
/// this to recover the container's identity from the workspace
/// path the user opened. The IPv4 address is returned alongside
/// the name so callers don't have to do a second `inspect` call —
/// `container ls` already has it.
///
/// The match is on the canonicalised path; symlink-equivalent
/// representations of the same workspace will match.
pub async fn find_container_by_mount_source(
    container_bin: &Path,
    local_path: &Path,
) -> Option<(String, String)> {
    use tokio::process::Command;

    let target = std::fs::canonicalize(local_path).unwrap_or_else(|_| local_path.to_path_buf());

    let fut = Command::new(container_bin)
        .arg("ls")
        .arg("--format")
        .arg("json")
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return None,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let arr = v.as_array()?;

    for obj in arr {
        let cfg = obj.get("configuration").unwrap_or(obj);
        let mounts = match cfg.get("mounts").and_then(|m| m.as_array()) {
            Some(m) => m,
            None => continue,
        };
        let matches = mounts.iter().any(|m| {
            m.get("source")
                .and_then(|s| s.as_str())
                .map(|s| {
                    let p = std::path::PathBuf::from(s);
                    let canon = std::fs::canonicalize(&p).unwrap_or(p);
                    canon == target
                })
                .unwrap_or(false)
        });
        if !matches {
            continue;
        }
        let name = obj
            .get("name")
            .or_else(|| obj.get("Name"))
            .or_else(|| cfg.get("id"))
            .or_else(|| obj.get("id"))
            .or_else(|| obj.get("ID"))
            .and_then(|n| n.as_str())?
            .to_string();
        // `networks[]` lives at the top of the inspect object, but
        // also commonly under `configuration.networks` — try both.
        let networks = obj
            .get("networks")
            .or_else(|| cfg.get("networks"))
            .and_then(|n| n.as_array());
        let ipv4 = networks
            .and_then(|nets| nets.first())
            .and_then(|n| n.get("ipv4Address"))
            .and_then(|v| v.as_str())
            .and_then(|raw| raw.split('/').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(ipv4) = ipv4 else { continue };
        return Some((name, ipv4));
    }
    None
}

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
