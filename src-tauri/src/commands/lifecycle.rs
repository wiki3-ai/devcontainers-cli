//! Container lifecycle commands. Each one resolves the workspace, picks
//! the selected runtime, and delegates to it. The actual lifecycle
//! orchestration (devcontainer.json parsing, hook execution, log
//! streaming) lands in step 7 of the conversion roadmap; for now these
//! commands are wired through to the runtime trait so the IPC surface is
//! complete and testable.

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::container::{ContainerState, RuntimeRegistry};
use crate::host::HostState;

#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerStatusDto {
    pub workspace_id: String,
    pub state: String,
    pub container_id: Option<String>,
    pub image_ref: Option<String>,
    pub error: Option<String>,
}

fn workspace_must_exist(state: &HostState, workspace_id: &str) -> Result<(), String> {
    let inner = state.inner.read();
    if inner.workspaces.iter().any(|w| w.id == workspace_id) {
        Ok(())
    } else {
        Err(format!("unknown workspace: {workspace_id}"))
    }
}

fn pending(workspace_id: &str, msg: &str) -> ContainerStatusDto {
    ContainerStatusDto {
        workspace_id: workspace_id.to_string(),
        state: state_to_string(ContainerState::Unknown),
        container_id: None,
        image_ref: None,
        error: Some(msg.to_string()),
    }
}

fn state_to_string(s: ContainerState) -> String {
    match s {
        ContainerState::Created => "created",
        ContainerState::Running => "running",
        ContainerState::Stopped => "stopped",
        ContainerState::Exited => "exited",
        ContainerState::Unknown => "unknown",
    }
    .into()
}

#[tauri::command]
pub async fn container_status(
    state: State<'_, HostState>,
    _registry: State<'_, RuntimeRegistry>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    workspace_must_exist(&state, &workspace_id)?;
    Ok(pending(
        &workspace_id,
        "lifecycle orchestrator lands in step 7 of the roadmap",
    ))
}

#[tauri::command]
pub async fn container_up(
    state: State<'_, HostState>,
    _registry: State<'_, RuntimeRegistry>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    workspace_must_exist(&state, &workspace_id)?;
    Ok(pending(&workspace_id, "container_up not yet implemented"))
}

#[tauri::command]
pub async fn container_stop(
    state: State<'_, HostState>,
    _registry: State<'_, RuntimeRegistry>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    workspace_must_exist(&state, &workspace_id)?;
    Ok(pending(&workspace_id, "container_stop not yet implemented"))
}

#[tauri::command]
pub async fn container_rebuild(
    state: State<'_, HostState>,
    _registry: State<'_, RuntimeRegistry>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    workspace_must_exist(&state, &workspace_id)?;
    Ok(pending(
        &workspace_id,
        "container_rebuild not yet implemented",
    ))
}

#[tauri::command]
pub async fn container_remove(
    state: State<'_, HostState>,
    _registry: State<'_, RuntimeRegistry>,
    workspace_id: String,
) -> Result<ContainerStatusDto, String> {
    workspace_must_exist(&state, &workspace_id)?;
    Ok(pending(
        &workspace_id,
        "container_remove not yet implemented",
    ))
}
