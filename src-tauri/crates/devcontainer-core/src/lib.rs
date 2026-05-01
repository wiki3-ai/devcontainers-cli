//! `devcontainer-core` — reusable engine extracted from Devcontainers.app.
//!
//! Layering:
//!   * [`container`] — runtime-agnostic [`ContainerRuntime`] trait plus
//!     in-tree backends (Apple Containers, Podman, Docker).
//!   * [`devcontainer`] — `devcontainer.json` translation
//!     ([`devcontainer::translate`]) and the [`LifecycleOrchestrator`]
//!     ([`devcontainer::lifecycle`]) that drives build/pull/create/start
//!     plus lifecycle hooks against a runtime.
//!   * [`events`] — the [`EventSink`] trait through which the orchestrator
//!     emits status and log events to its host. The Tauri 2 binary in
//!     `src-tauri/src/tauri_sink.rs` provides one impl; embedding apps
//!     supply their own.
//!
//! No `tauri` dependency: this crate is consumed both by the
//! Devcontainers.app binary and by sibling Tauri apps (e.g. Wiki3).

pub mod container;
pub mod devcontainer;
pub mod events;

pub use container::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerState,
    ContainerStatus, ExecOptions, ExecResult, ImageRef, LogChunk, LogOptions, LogStream,
    LogStreamKind, MountKind, MountSpec, PortForward, PortProtocol, RuntimeAvailability, RuntimeId,
    RuntimeRegistry,
};
pub use devcontainer::lifecycle::{
    LifecycleError, LifecycleOrchestrator, LifecycleStatus, LABEL_CONFIG_HASH,
};
pub use devcontainer::proxy_manager::ProxyManager;
pub use devcontainer::translate::{DevContainerBuild, LifecycleCommand, ParsedDevContainer};
pub use events::EventSink;

// Re-exported so app crates can return proxy stats from Tauri
// commands without depending on `devcontainer-proxy` directly.
pub use devcontainer_proxy::{HostStats, ProxyStatsSnapshot};
