//! Lifecycle orchestrator. Step 7 of the conversion roadmap fleshes this
//! out — running `initializeCommand` on the host, then driving the
//! `onCreate`/`updateContent`/`postCreate`/`postStart`/`postAttach` hooks
//! against the selected `ContainerRuntime`, streaming logs to the WebView
//! via `devcontainer://log` Tauri events.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleHook {
    InitializeCommand,
    OnCreateCommand,
    UpdateContentCommand,
    PostCreateCommand,
    PostStartCommand,
    PostAttachCommand,
}

impl LifecycleHook {
    /// Whether the hook runs on the *host* (true) or inside the container.
    pub fn runs_on_host(self) -> bool {
        matches!(self, Self::InitializeCommand)
    }
}
