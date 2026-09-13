//! Runtime-agnostic container backend.
//!
//! [`ContainerRuntime`] is the seam through which the lifecycle orchestrator
//! drives any container engine. Implementations live in the submodules
//! (`apple_containers`, `podman`, `docker`); selection is performed by
//! [`RuntimeRegistry`] based on user setting and capability probing.

pub mod apple_containers;
pub mod docker;
pub mod exec_probe;
pub mod podman;
pub mod traits;

pub use docker::DockerRuntime;
pub use exec_probe::{probe_binary, ExecutableProbe};
pub use traits::{
    BuildSpec, CompatibilityIssue, CompatibilityReport, CompatibilitySeverity, ContainerRuntime,
    ContainerRuntimeError, ContainerSpec, ContainerState, ContainerStatus, ExecOptions, ExecResult,
    ImageRef, LogChunk, LogOptions, LogStream, LogStreamKind, MountKind, MountSpec, PortForward,
    PortProtocol, RuntimeAvailability, RuntimeId,
};

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::{debug, warn};

/// Preference order used when the user has not chosen a runtime: whichever
/// of these is actually installed gets used.
///
/// Apple Containers is last deliberately. It works, but it is the most
/// restricted backend — it rejects `runArgs` flags it does not implement —
/// so it should only be picked when nothing better is present. This is
/// policy, kept out of the backends themselves.
pub const DEFAULT_PREFERENCE: &[RuntimeId] = &[
    RuntimeId::Docker,
    RuntimeId::Podman,
    RuntimeId::AppleContainers,
];

/// Holds all known runtime backends, the user's explicit choice (if any),
/// and a memoised result of the availability probe.
pub struct RuntimeRegistry {
    backends: HashMap<RuntimeId, Arc<dyn ContainerRuntime>>,
    /// Explicit user choice. `None` means "decide by availability".
    choice: RwLock<Option<RuntimeId>>,
    /// Memoised automatic resolution, so `resolve()` does not spawn a probe
    /// process on every call.
    auto: RwLock<Option<RuntimeId>>,
}

impl RuntimeRegistry {
    /// Build a registry with all in-tree backends and no explicit choice,
    /// so [`Self::resolve`] picks the first available one.
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
            choice: RwLock::new(None),
            auto: RwLock::new(None),
        }
    }

    pub fn list(&self) -> Vec<Arc<dyn ContainerRuntime>> {
        self.backends.values().cloned().collect()
    }

    pub fn get(&self, id: RuntimeId) -> Option<Arc<dyn ContainerRuntime>> {
        self.backends.get(&id).cloned()
    }

    /// Pin the runtime explicitly. This always wins over the automatic
    /// choice, so picking Apple Containers keeps it even when Docker is
    /// installed.
    pub fn select(&self, id: RuntimeId) -> Result<(), ContainerRuntimeError> {
        if !self.backends.contains_key(&id) {
            return Err(ContainerRuntimeError::Unsupported(format!(
                "runtime {id:?} is not registered"
            )));
        }
        *self.choice.write() = Some(id);
        Ok(())
    }

    /// Drop the explicit choice and let availability decide again.
    pub fn clear_selection(&self) {
        *self.choice.write() = None;
        *self.auto.write() = None;
    }

    /// The runtime an operation should use.
    ///
    /// An explicit selection always wins. Otherwise the first runtime in
    /// [`DEFAULT_PREFERENCE`] that probes as available is used, and the
    /// answer is memoised. If none is available we still hand back the top
    /// preference, so the caller gets that backend's own "not installed" /
    /// "daemon down" error instead of a vague failure here.
    pub async fn resolve(&self) -> Arc<dyn ContainerRuntime> {
        if let Some(id) = *self.choice.read() {
            return self.backend(id);
        }
        if let Some(id) = *self.auto.read() {
            return self.backend(id);
        }
        for id in DEFAULT_PREFERENCE {
            let Some(backend) = self.backends.get(id).cloned() else {
                continue;
            };
            match backend.probe().await {
                Ok(availability) if availability.available => {
                    debug!(runtime = ?id, "runtime selected by availability");
                    *self.auto.write() = Some(*id);
                    return backend;
                }
                Ok(availability) => {
                    debug!(runtime = ?id, reason = ?availability.reason, "runtime unavailable");
                }
                Err(e) => debug!(runtime = ?id, "runtime probe failed: {e}"),
            }
        }
        let fallback = DEFAULT_PREFERENCE[0];
        warn!(runtime = ?fallback, "no container runtime available; using preferred default");
        *self.auto.write() = Some(fallback);
        self.backend(fallback)
    }

    /// The explicitly chosen runtime, if the user has chosen one. `None`
    /// means the registry is deciding by availability.
    pub fn selected_id(&self) -> Option<RuntimeId> {
        *self.choice.read()
    }

    fn backend(&self, id: RuntimeId) -> Arc<dyn ContainerRuntime> {
        self.backends
            .get(&id)
            .cloned()
            .expect("runtime id must be registered")
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
            choice: RwLock::new(Some(id)),
            auto: RwLock::new(Some(id)),
        }
    }
}
