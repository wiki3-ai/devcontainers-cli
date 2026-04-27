//! Runtime-agnostic types and the [`ContainerRuntime`] trait.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStatus {
    pub container_id: String,
    pub state: ContainerState,
    pub image_ref: Option<String>,
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

    async fn pull(&self, image: &ImageRef) -> Result<(), ContainerRuntimeError>;
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeAvailability {
    pub available: bool,
    pub version: Option<String>,
    pub reason: Option<String>,
}
