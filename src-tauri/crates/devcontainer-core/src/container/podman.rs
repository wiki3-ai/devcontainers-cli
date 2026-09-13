//! Podman backend.
//!
//! Podman's CLI is intentionally Docker-compatible for every operation the
//! orchestrator performs — `create`, `start`, `build`, `exec`, `logs`,
//! `inspect` and all their flags match — so this backend drives
//! [`DockerRuntime`] rather than duplicating it, overriding only the three
//! things that genuinely differ:
//!
//! * the [`RuntimeId`] it reports,
//! * the binary it probes, and
//! * how "is the engine up?" is answered. Podman runs containers inside a VM
//!   (`podman machine`) rather than behind a daemon, so the check is
//!   `podman info` and the remedy is `podman machine start`.
//!
//! Sharing the mechanics is deliberate, not accidental coupling: these two
//! engines really do speak the same command language, and a bug fixed in the
//! Docker path should not have to be fixed twice. Where a Podman release
//! diverges on a specific operation, that operation gets its own
//! implementation here and the shared one is left alone.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::docker::{run_capturing, DockerRuntime};
use super::exec_probe;
use super::traits::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerStatus,
    ExecOptions, ExecResult, ImageRef, LogChunk, LogOptions, LogStream, LogStreamKind,
    RuntimeAvailability, RuntimeId,
};

/// Locations probed before falling back to `PATH`.
///
/// Podman's macOS installer puts the CLI under `/opt/podman/bin`, which is
/// not on the minimal `PATH` a GUI-launched app inherits — so probing known
/// locations matters here even more than it does for Docker.
const PODMAN_STANDARD_PATHS: &[&str] = &[
    "/opt/podman/bin/podman",
    "/usr/local/bin/podman",
    "/opt/homebrew/bin/podman",
];

/// Podman's install locations on Windows, most preferred first.
///
/// The installer writes to `%ProgramFiles%\RedHat\Podman`, with the CLI in
/// `bin`; a per-user `podman machine` install can land under
/// `%LOCALAPPDATA%\Programs`.
#[cfg(windows)]
fn windows_podman_install_paths() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut out = Vec::new();
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        out.push(
            PathBuf::from(pf)
                .join("RedHat")
                .join("Podman")
                .join("bin")
                .join("podman.exe"),
        );
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(
            PathBuf::from(local)
                .join("Programs")
                .join("RedHat")
                .join("Podman")
                .join("bin")
                .join("podman.exe"),
        );
    }
    out
}

/// No Windows-specific locations on other platforms.
#[cfg(not(windows))]
fn windows_podman_install_paths() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Every location to probe, most preferred first.
fn podman_standard_paths() -> Vec<std::path::PathBuf> {
    PODMAN_STANDARD_PATHS
        .iter()
        .map(std::path::PathBuf::from)
        .chain(windows_podman_install_paths())
        .collect()
}

#[derive(Debug, Clone)]
pub struct PodmanRuntime {
    inner: DockerRuntime,
}

impl Default for PodmanRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl PodmanRuntime {
    /// Resolve the `podman` binary via the standard install locations and
    /// `PATH`, falling back to the bare name so error messages still make
    /// sense when nothing was found.
    pub fn new() -> Self {
        let binary = exec_probe::probe_binary_in_env("podman", &podman_standard_paths())
            .path_str()
            .map(str::to_owned)
            .unwrap_or_else(|| "podman".to_string());
        Self {
            inner: DockerRuntime::with_binary(binary),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            inner: DockerRuntime::with_binary(binary),
        }
    }

    /// Whether a `podman` executable exists on this host. Cheap, and
    /// deliberately does not require the machine VM to be running — see
    /// [`ContainerRuntime::ensure_system_running`] for that distinction.
    pub fn detect() -> exec_probe::ExecutableProbe {
        exec_probe::probe_binary_in_env("podman", &podman_standard_paths())
    }
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::Podman
    }

    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
        // `podman --version` needs only the CLI. Whether the machine VM is up
        // is a separate question, answered by `ensure_system_running`.
        match run_capturing(self.inner.cli(), ["--version"]).await {
            Ok(stdout) => Ok(RuntimeAvailability {
                available: true,
                version: Some(stdout.trim().to_string()).filter(|s| !s.is_empty()),
                reason: None,
            }),
            Err(e) => Ok(RuntimeAvailability {
                available: false,
                version: None,
                reason: Some(format!("`{}` not runnable: {e}", self.inner.cli().binary())),
            }),
        }
    }

    /// Podman keeps its containers in a VM, so "system running" means
    /// `podman machine` is up rather than a daemon being reachable.
    ///
    /// As with Docker we deliberately do not start it ourselves: booting a VM
    /// is slow and visible enough that it should be the user's call. Say what
    /// is wrong, and stop.
    async fn ensure_system_running(
        &self,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<(), ContainerRuntimeError> {
        if run_capturing(
            self.inner.cli(),
            ["info", "--format", "{{.Version.Version}}"],
        )
        .await
        .is_ok()
        {
            return Ok(());
        }
        let message = format!(
            "the Podman machine is not reachable via `{}`. Start it with \
             `podman machine start` and try again.",
            self.inner.cli().binary()
        );
        if let Some(tx) = log_sink {
            let _ = tx
                .send(LogChunk {
                    stream: LogStreamKind::System,
                    line: message.clone(),
                })
                .await;
        }
        Err(ContainerRuntimeError::Unavailable(message))
    }

    // --- shared Docker-compatible surface -------------------------------
    //
    // These all delegate. When Podman genuinely diverges on one of them,
    // implement it here instead of changing the shared behaviour.

    async fn image_exists(&self, image: &ImageRef) -> Result<bool, ContainerRuntimeError> {
        self.inner.image_exists(image).await
    }

    async fn image_label(
        &self,
        image: &ImageRef,
        key: &str,
    ) -> Result<Option<String>, ContainerRuntimeError> {
        self.inner.image_label(image, key).await
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), ContainerRuntimeError> {
        self.inner.pull(image).await
    }

    async fn build(
        &self,
        spec: &BuildSpec,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<ImageRef, ContainerRuntimeError> {
        self.inner.build(spec, log_sink).await
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, ContainerRuntimeError> {
        self.inner.create(spec).await
    }

    async fn start(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        self.inner.start(container_id).await
    }

    async fn stop(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        self.inner.stop(container_id).await
    }

    async fn remove(&self, container_id: &str, force: bool) -> Result<(), ContainerRuntimeError> {
        self.inner.remove(container_id, force).await
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerStatus, ContainerRuntimeError> {
        self.inner.inspect(container_id).await
    }

    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
        self.inner.list().await
    }

    async fn exec(
        &self,
        container_id: &str,
        options: &ExecOptions,
    ) -> Result<ExecResult, ContainerRuntimeError> {
        self.inner.exec(container_id, options).await
    }

    async fn logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> Result<LogStream, ContainerRuntimeError> {
        self.inner.logs(container_id, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::docker::map_state;
    use crate::container::ContainerState;

    #[test]
    fn reports_its_own_runtime_id() {
        // The wrapper exists for this: driving the shared implementation must
        // not make the registry think it is talking to Docker.
        assert_eq!(PodmanRuntime::new().id(), RuntimeId::Podman);
        assert_eq!(PodmanRuntime::with_binary("podman").id(), RuntimeId::Podman);
    }

    #[test]
    fn map_state_understands_podmans_stopped() {
        // Podman says `stopped` where Docker says `exited`. Without this the
        // dashboard would show `Unknown` for a plainly stopped container.
        assert_eq!(map_state(Some("stopped")), ContainerState::Stopped);
        assert_eq!(map_state(Some("running")), ContainerState::Running);
        assert_eq!(map_state(Some("created")), ContainerState::Created);
        assert_eq!(map_state(Some("exited")), ContainerState::Exited);
    }

    #[test]
    fn probes_podmans_own_install_paths() {
        // Guards against someone "simplifying" this to Docker's path list,
        // which would find nothing on a Podman-only host.
        assert!(PODMAN_STANDARD_PATHS.contains(&"/opt/podman/bin/podman"));
    }
}
