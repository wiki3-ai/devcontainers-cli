//! Runtime-agnostic container backend.
//!
//! [`ContainerRuntime`] is the seam through which the lifecycle orchestrator
//! drives any container engine. Implementations live in the submodules
//! (`apple_containers`, `podman`, `docker`); selection is performed by
//! [`RuntimeRegistry`] based on user setting and capability probing.

pub mod apple_containers;
pub mod docker;
pub mod podman;
pub mod traits;

pub use traits::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerState,
    ContainerStatus, ExecOptions, ExecResult, ImageRef, LogChunk, LogOptions, LogStream,
    LogStreamKind, MountKind, MountSpec, PortForward, PortProtocol, RuntimeAvailability, RuntimeId,
};

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// Holds all known runtime backends and the currently selected one.
pub struct RuntimeRegistry {
    backends: HashMap<RuntimeId, Arc<dyn ContainerRuntime>>,
    selected: RwLock<RuntimeId>,
}

impl RuntimeRegistry {
    /// Build a registry with all in-tree backends. Apple Containers is the
    /// default on macOS; Podman/Docker are stubs that report unavailable.
    pub fn with_default_backends() -> Self {
        let mut backends: HashMap<RuntimeId, Arc<dyn ContainerRuntime>> = HashMap::new();
        backends.insert(
            RuntimeId::AppleContainers,
            Arc::new(apple_containers::AppleContainersRuntime::new()),
        );
        backends.insert(RuntimeId::Podman, Arc::new(podman::PodmanRuntime::new()));
        backends.insert(RuntimeId::Docker, Arc::new(docker::DockerRuntime::new()));
        Self {
            backends,
            selected: RwLock::new(RuntimeId::AppleContainers),
        }
    }

    pub fn list(&self) -> Vec<Arc<dyn ContainerRuntime>> {
        self.backends.values().cloned().collect()
    }

    pub fn get(&self, id: RuntimeId) -> Option<Arc<dyn ContainerRuntime>> {
        self.backends.get(&id).cloned()
    }

    pub fn select(&self, id: RuntimeId) -> Result<(), ContainerRuntimeError> {
        if !self.backends.contains_key(&id) {
            return Err(ContainerRuntimeError::Unsupported(format!(
                "runtime {id:?} is not registered"
            )));
        }
        *self.selected.write() = id;
        Ok(())
    }

    pub fn selected(&self) -> Arc<dyn ContainerRuntime> {
        let id = *self.selected.read();
        self.backends
            .get(&id)
            .cloned()
            .expect("selected runtime must be registered")
    }

    /// Test helper: build a registry with exactly one backend, already
    /// selected. Lets the lifecycle orchestrator be exercised end-to-end
    /// against a fake `ContainerRuntime` without touching the real
    /// Apple/Podman/Docker backends.
    #[doc(hidden)]
    pub fn with_single(id: RuntimeId, backend: Arc<dyn ContainerRuntime>) -> Self {
        let mut backends: HashMap<RuntimeId, Arc<dyn ContainerRuntime>> = HashMap::new();
        backends.insert(id, backend);
        Self {
            backends,
            selected: RwLock::new(id),
        }
    }
}
