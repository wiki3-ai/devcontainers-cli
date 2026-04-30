//! Runtime-agnostic types and the [`ContainerRuntime`] trait.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeId {
    AppleContainers,
    Podman,
    Docker,
}

#[derive(Debug, Error)]
pub enum ContainerRuntimeError {
    #[error("runtime unavailable: {0}")]
    Unavailable(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend reported failure: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRef {
    pub registry: Option<String>,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    pub kind: MountKind,
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountKind {
    Bind,
    Volume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

/// Runtime-agnostic input for `create` derived from a parsed
/// `devcontainer.json`. The lifecycle orchestrator (see
/// `crate::devcontainer::lifecycle`) is responsible for translating the
/// spec into this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub name: String,
    pub image: ImageRef,
    pub command: Option<Vec<String>>,
    pub workdir: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub mounts: Vec<MountSpec>,
    pub ports: Vec<PortForward>,
    pub user: Option<String>,
    pub privileged: bool,
    /// Verbatim extra arguments to pass to the runtime's `create`
    /// invocation, sourced from the devcontainer.json `runArgs` field.
    /// These are inserted immediately before the image reference so they
    /// behave like `docker run` flags. The runtime is free to reject
    /// flags it does not understand.
    #[serde(default)]
    pub run_args: Vec<String>,
    /// Labels stamped onto the container at create time. The orchestrator
    /// uses these to remember the config-hash and other facts that need
    /// to survive an app restart without local state.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Runtime-agnostic input for [`ContainerRuntime::build`]. The
/// lifecycle orchestrator translates the parsed `devcontainer.json`
/// build stanza into this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSpec {
    /// Tag to apply to the resulting image.
    pub tag: ImageRef,
    /// Absolute path to the build context directory.
    pub context_dir: PathBuf,
    /// Absolute path to the Dockerfile.
    pub dockerfile: PathBuf,
    /// `--build-arg` key/value pairs.
    pub build_args: HashMap<String, String>,
    /// Multi-stage build target.
    pub target: Option<String>,
    /// `--label` key/value pairs stamped onto the resulting image.
    /// Used by the orchestrator to record the config-hash so a future
    /// `up` can decide whether the cached image is still current.
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStatus {
    pub container_id: String,
    pub state: ContainerState,
    pub image_ref: Option<String>,
    /// Host-side paths bind-mounted into the container, in the order the
    /// runtime reported them. Used by the dashboard to link containers
    /// back to known workspace folders without relying on a name
    /// convention.
    #[serde(default)]
    pub host_mounts: Vec<String>,
    /// Labels reported by the runtime for this container. The
    /// orchestrator looks up `org.devcontainers.config_hash` and
    /// related keys here to detect drift without storing app-side
    /// state. Empty when the runtime does not expose labels (e.g.,
    /// older Apple `container` builds).
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContainerState {
    Created,
    Running,
    Stopped,
    Exited,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub command: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub user: Option<String>,
    pub tty: bool,
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Which stream a [`LogChunk`] came from. The `system` variant is reserved
/// for orchestrator-injected log lines (e.g. "starting container…") that the
/// runtime did not produce itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStreamKind {
    Stdout,
    Stderr,
    System,
}

/// One line of container output, as produced by `runtime.logs(...)` or by
/// the lifecycle orchestrator's hook execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogChunk {
    pub stream: LogStreamKind,
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Stream new output as it is produced. When false, returns historical
    /// logs and closes the channel.
    pub follow: bool,
    /// If set, only the last `tail` lines of historical output are returned.
    pub tail: Option<usize>,
}

/// Streaming log handle. Dropping the underlying receiver should be enough
/// to cause the backend to abort its log task; implementations are expected
/// to detect a closed channel and stop the child process.
pub type LogStream = mpsc::Receiver<LogChunk>;

/// The seam through which the lifecycle orchestrator drives any container
/// engine. All operations are async; long-running ones (logs, events,
/// `exec` with PTY) belong with the streaming APIs added in step 7.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    fn id(&self) -> RuntimeId;

    /// Probe whether the backend is available on this host. Implementations
    /// must not panic on missing tools — they should return `Ok(false)` and
    /// optionally a `reason`.
    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError>;

    /// Ensure any out-of-process daemon/services this backend depends on
    /// are running, starting them if necessary. Idempotent. The default
    /// is a no-op for backends that have no separate service component.
    /// Optional `log_sink` receives system-stream messages so callers
    /// can surface "starting container system…" in the UI.
    async fn ensure_system_running(
        &self,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<(), ContainerRuntimeError> {
        let _ = log_sink;
        Ok(())
    }

    /// Whether an image with this reference is already present locally.
    /// Backends that cannot answer cheaply may always return `false`,
    /// which makes callers fall through to a real `pull`/`build`.
    async fn image_exists(&self, image: &ImageRef) -> Result<bool, ContainerRuntimeError> {
        let _ = image;
        Ok(false)
    }

    /// Read the value of an OCI image config label. Returns `Ok(None)`
    /// when the image is missing or the label is not set. Used by the
    /// orchestrator to decide whether a cached image still matches the
    /// current devcontainer.json + Dockerfile fingerprint.
    async fn image_label(
        &self,
        image: &ImageRef,
        key: &str,
    ) -> Result<Option<String>, ContainerRuntimeError> {
        let _ = (image, key);
        Ok(None)
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), ContainerRuntimeError>;
    /// Build an image from a Dockerfile. Backends that do not support
    /// building must return [`ContainerRuntimeError::Unsupported`]. The
    /// optional `log_sink` receives stdout/stderr lines as the build
    /// progresses; the same channel pattern as [`Self::logs`] applies
    /// (closing the receiver tells the backend to abort).
    async fn build(
        &self,
        spec: &BuildSpec,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<ImageRef, ContainerRuntimeError> {
        let _ = (spec, log_sink);
        Err(ContainerRuntimeError::Unsupported(format!(
            "{:?} backend does not support building images",
            self.id()
        )))
    }
    async fn create(&self, spec: &ContainerSpec) -> Result<String, ContainerRuntimeError>;
    async fn start(&self, container_id: &str) -> Result<(), ContainerRuntimeError>;
    async fn stop(&self, container_id: &str) -> Result<(), ContainerRuntimeError>;
    async fn remove(&self, container_id: &str, force: bool) -> Result<(), ContainerRuntimeError>;
    async fn inspect(&self, container_id: &str) -> Result<ContainerStatus, ContainerRuntimeError>;
    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError>;
    async fn exec(
        &self,
        container_id: &str,
        options: &ExecOptions,
    ) -> Result<ExecResult, ContainerRuntimeError>;

    /// Stream container logs. When `options.follow` is true, the returned
    /// receiver yields lines until the container exits or the receiver is
    /// dropped (which should abort the backend's log task).
    async fn logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> Result<LogStream, ContainerRuntimeError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeAvailability {
    pub available: bool,
    pub version: Option<String>,
    pub reason: Option<String>,
}
