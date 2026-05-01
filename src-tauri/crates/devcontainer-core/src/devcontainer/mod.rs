//! Devcontainer-specific orchestration: translate the parsed `devcontainer.json`
//! (from the WebView-side spec slice) into a runtime-agnostic
//! [`crate::container::ContainerSpec`] and drive lifecycle hooks against
//! the selected [`crate::container::ContainerRuntime`].

pub mod lifecycle;
pub mod proxy_manager;
pub mod translate;
