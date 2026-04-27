//! Runtime selection and capability probing.

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::container::{RuntimeId, RuntimeRegistry};

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
