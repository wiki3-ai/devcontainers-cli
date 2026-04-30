//! Container lifecycle commands. Each one resolves the workspace, picks
//! the selected runtime, and delegates to the
//! [`crate::devcontainer::lifecycle::LifecycleOrchestrator`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::container::RuntimeRegistry;
use crate::devcontainer::lifecycle::{LifecycleOrchestrator, LifecycleStatus};
use crate::devcontainer::translate::ParsedDevContainer;
use crate::host::HostState;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatusDto {
    pub workspace_id: String,
    pub state: String,
    pub container_id: Option<String>,
    pub image_ref: Option<String>,
    pub error: Option<String>,
    /// `true` when the running container's stamped config_hash label
    /// disagrees with the on-disk devcontainer.json/Dockerfile.
    /// Omitted when undecidable (no live container, no label, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_drift: Option<bool>,
}

impl From<LifecycleStatus> for ContainerStatusDto {
    fn from(s: LifecycleStatus) -> Self {
        Self {
            workspace_id: s.workspace_id,
            state: s.state.to_string(),
            container_id: s.container_id,
            image_ref: s.image_ref,
            error: s.error,
            config_drift: s.config_drift,
        }
    }
}

fn resolve_workspace(state: &HostState, workspace_id: &str) -> Result<PathBuf, String> {
    let inner = state.inner.read();
    inner
        .workspaces
        .iter()
        .find(|w| w.id == workspace_id)
        .map(|w| w.path.clone())
        .ok_or_else(|| format!("unknown workspace: {workspace_id}"))
}

#[tauri::command]
pub async fn submit_parsed_devcontainer(
    state: State<'_, HostState>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
    parsed: ParsedDevContainer,
) -> Result<(), String> {
    resolve_workspace(&state, &workspace_id)?;
    orchestrator.set_parsed_config(&workspace_id, parsed);
    Ok(())
}

#[tauri::command]
pub async fn container_status(
    state: State<'_, HostState>,
    registry: State<'_, RuntimeRegistry>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    let path = resolve_workspace(&state, &workspace_id)?;
    Ok(orchestrator
        .status_with_drift(&registry, &workspace_id, &path)
        .await
        .into())
}

#[tauri::command]
pub async fn container_up(
    app: AppHandle,
    state: State<'_, HostState>,
    registry: State<'_, RuntimeRegistry>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    let path = resolve_workspace(&state, &workspace_id)?;
    orchestrator
        .up(&app, &registry, &workspace_id, &path)
        .await
        .map(ContainerStatusDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn container_stop(
    app: AppHandle,
    state: State<'_, HostState>,
    registry: State<'_, RuntimeRegistry>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    resolve_workspace(&state, &workspace_id)?;
    orchestrator
        .stop(&app, &registry, &workspace_id)
        .await
        .map(ContainerStatusDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn container_rebuild(
    app: AppHandle,
    state: State<'_, HostState>,
    registry: State<'_, RuntimeRegistry>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    let path = resolve_workspace(&state, &workspace_id)?;
    orchestrator
        .rebuild(&app, &registry, &workspace_id, &path)
        .await
        .map(ContainerStatusDto::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn container_remove(
    app: AppHandle,
    state: State<'_, HostState>,
    registry: State<'_, RuntimeRegistry>,
    orchestrator: State<'_, LifecycleOrchestrator>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    resolve_workspace(&state, &workspace_id)?;
    orchestrator
        .remove(&app, &registry, &workspace_id)
        .await
        .map(ContainerStatusDto::from)
        .map_err(|e| e.to_string())
}
