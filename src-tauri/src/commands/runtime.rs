//! Runtime selection and capability probing.

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::container::{ContainerState, RuntimeId, RuntimeRegistry};

#[derive(Debug, Serialize, Deserialize)]
pub struct RuntimeInfoDto {
    pub id: RuntimeId,
    pub available: bool,
    pub version: Option<String>,
    pub reason: Option<String>,
}

#[tauri::command]
pub async fn list_runtimes(
    registry: State<'_, RuntimeRegistry>,
) -> Result<Vec<RuntimeInfoDto>, String> {
    let mut out = Vec::new();
    for backend in registry.list() {
        let info = backend.probe().await.map_err(|e| e.to_string())?;
        out.push(RuntimeInfoDto {
            id: backend.id(),
            available: info.available,
            version: info.version,
            reason: info.reason,
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn select_runtime(
    registry: State<'_, RuntimeRegistry>,
    id: RuntimeId,
) -> Result<(), String> {
    registry.select(id).map_err(|e| e.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerEntry {
    pub container_id: String,
    pub state: String,
    pub image_ref: Option<String>,
    /// Host-side bind-mount sources, used by the dashboard to link a
    /// container back to a known repo without relying on container
    /// names.
    pub host_mounts: Vec<String>,
}

/// List all containers known to the selected runtime, regardless of
/// whether the dashboard knows about them. The dashboard joins this with
/// its workspace list to render linked-container badges and a separate
/// "unlinked containers" section.
#[tauri::command]
pub async fn list_containers(
    registry: State<'_, RuntimeRegistry>,
) -> Result<Vec<ContainerEntry>, String> {
    let runtime = registry.selected();
    let statuses = runtime.list().await.map_err(|e| e.to_string())?;
    Ok(statuses
        .into_iter()
        .map(|s| ContainerEntry {
            container_id: s.container_id,
            state: state_str(s.state).to_string(),
            image_ref: s.image_ref,
            host_mounts: s.host_mounts,
        })
        .collect())
}

fn state_str(s: ContainerState) -> &'static str {
    match s {
        ContainerState::Created => "created",
        ContainerState::Running => "running",
        ContainerState::Stopped => "stopped",
        ContainerState::Exited => "exited",
        ContainerState::Unknown => "unknown",
    }
}

#[tauri::command]
pub async fn container_start_by_id(
    registry: State<'_, RuntimeRegistry>,
    container_id: String,
) -> Result<(), String> {
    let runtime = registry.selected();
    runtime
        .start(&container_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn container_stop_by_id(
    registry: State<'_, RuntimeRegistry>,
    container_id: String,
) -> Result<(), String> {
    let runtime = registry.selected();
    runtime.stop(&container_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn container_remove_by_id(
    registry: State<'_, RuntimeRegistry>,
    container_id: String,
    force: bool,
) -> Result<(), String> {
    let runtime = registry.selected();
    runtime
        .remove(&container_id, force)
        .await
        .map_err(|e| e.to_string())
}
