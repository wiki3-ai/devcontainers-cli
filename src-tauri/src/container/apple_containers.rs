//! Apple Containers backend.
//!
//! v1 drives the `container` CLI bundled with macOS 26+. Where a Swift/C
//! programmatic API exists in a future macOS release, the implementation
//! should switch to it; until then we shell out to `container …` and parse
//! the JSON output. This module is a stub — real `pull/create/start/exec/
//! logs/stop/remove` lands in step 5 of the conversion roadmap.

use async_trait::async_trait;

use super::traits::{
    ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerStatus, ExecOptions,
    ExecResult, ImageRef, RuntimeAvailability, RuntimeId,
};

#[derive(Debug, Default)]
pub struct AppleContainersRuntime {}

impl AppleContainersRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ContainerRuntime for AppleContainersRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::AppleContainers
    }

    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
        // On non-mac hosts the runtime is definitively unavailable.
        #[cfg(not(target_os = "macos"))]
        {
            return Ok(RuntimeAvailability {
                available: false,
                version: None,
                reason: Some("Apple Containers requires macOS 26 or later".into()),
            });
        }
        #[cfg(target_os = "macos")]
        {
            // Real implementation: `container --version`. Stubbed for now.
            Ok(RuntimeAvailability {
                available: false,
                version: None,
                reason: Some("not yet implemented (step 5 of the conversion roadmap)".into()),
            })
        }
    }

    async fn pull(&self, _image: &ImageRef) -> Result<(), ContainerRuntimeError> {
        Err(unimplemented_err("pull"))
    }
    async fn create(&self, _spec: &ContainerSpec) -> Result<String, ContainerRuntimeError> {
        Err(unimplemented_err("create"))
    }
    async fn start(&self, _container_id: &str) -> Result<(), ContainerRuntimeError> {
        Err(unimplemented_err("start"))
    }
    async fn stop(&self, _container_id: &str) -> Result<(), ContainerRuntimeError> {
        Err(unimplemented_err("stop"))
    }
    async fn remove(&self, _container_id: &str, _force: bool) -> Result<(), ContainerRuntimeError> {
        Err(unimplemented_err("remove"))
    }
    async fn inspect(&self, _container_id: &str) -> Result<ContainerStatus, ContainerRuntimeError> {
        Err(unimplemented_err("inspect"))
    }
    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
        Err(unimplemented_err("list"))
    }
    async fn exec(
        &self,
        _container_id: &str,
        _options: &ExecOptions,
    ) -> Result<ExecResult, ContainerRuntimeError> {
        Err(unimplemented_err("exec"))
    }
}

fn unimplemented_err(op: &'static str) -> ContainerRuntimeError {
    ContainerRuntimeError::Unsupported(format!("apple-containers `{op}` not implemented yet"))
}
