//! Workspace CRUD commands, persisted in [`HostState`].

use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use crate::host::{HostState, Workspace};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
    pub id: String,
    pub path: String,
    pub display_name: String,
    #[serde(default)]
    pub last_opened_at: Option<String>,
}

impl From<&Workspace> for WorkspaceDto {
    fn from(w: &Workspace) -> Self {
        Self {
            id: w.id.clone(),
            path: w.path.display().to_string(),
            display_name: w.display_name.clone(),
            last_opened_at: w.last_opened_at.clone(),
        }
    }
}

#[tauri::command]
pub async fn list_workspaces(state: State<'_, HostState>) -> Result<Vec<WorkspaceDto>, String> {
    let inner = state.inner.read();
    Ok(inner.workspaces.iter().map(WorkspaceDto::from).collect())
}

#[tauri::command]
pub async fn add_workspace(
    state: State<'_, HostState>,
    path: String,
) -> Result<WorkspaceDto, String> {
    let path_buf = std::path::PathBuf::from(&path);
    if !path_buf.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let display_name = path_buf
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string();
    let workspace = Workspace {
        id: Uuid::new_v4().to_string(),
        path: path_buf,
        display_name,
        last_opened_at: None,
    };
    let dto = WorkspaceDto::from(&workspace);
    {
        let mut inner = state.inner.write();
        inner.workspaces.push(workspace);
    }
    state.save().map_err(|e| e.to_string())?;
    Ok(dto)
}

#[tauri::command]
pub async fn remove_workspace(state: State<'_, HostState>, id: String) -> Result<(), String> {
    {
        let mut inner = state.inner.write();
        inner.workspaces.retain(|w| w.id != id);
    }
    state.save().map_err(|e| e.to_string())?;
    Ok(())
}
