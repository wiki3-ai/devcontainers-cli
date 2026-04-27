//! Podman backend — placeholder. The runtime reports `Unavailable` until
//! Phase 3 of the conversion roadmap implements it.

use async_trait::async_trait;

use super::traits::{
    ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerStatus, ExecOptions,
    ExecResult, ImageRef, RuntimeAvailability, RuntimeId,
};

#[derive(Debug, Default)]
pub struct PodmanRuntime {}

impl PodmanRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::Podman
    }

    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
        Ok(RuntimeAvailability {
            available: false,
            version: None,
            reason: Some("Podman backend lands in Phase 3 of the conversion roadmap".into()),
        })
    }

    async fn pull(&self, _: &ImageRef) -> Result<(), ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn create(&self, _: &ContainerSpec) -> Result<String, ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn start(&self, _: &str) -> Result<(), ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn stop(&self, _: &str) -> Result<(), ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn remove(&self, _: &str, _: bool) -> Result<(), ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn inspect(&self, _: &str) -> Result<ContainerStatus, ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
        Err(unsupported())
    }
    async fn exec(&self, _: &str, _: &ExecOptions) -> Result<ExecResult, ContainerRuntimeError> {
        Err(unsupported())
    }
}

fn unsupported() -> ContainerRuntimeError {
    ContainerRuntimeError::Unsupported("Podman backend not implemented".into())
}
