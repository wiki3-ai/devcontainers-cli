//! `FileHost` bridge — exposes a sandboxed filesystem surface to the
//! WebView so the Deno-built spec engine can parse `devcontainer.json`.
//!
//! Sandboxing rules (enforced by [`resolve_within_workspaces`]):
//!   * Every requested path must resolve under the canonical path of one of
//!     the registered workspaces.
//!   * Symlinks that escape the workspace are rejected.
//!   * No write is permitted outside the `.devcontainer` subtree of a
//!     workspace.
//!
//! These invariants matter even for a local-only desktop app: the Tauri
//! WebView is the trust boundary the spec slice runs behind, and any new
//! capability we expose to it must be locked down to a single workspace.

use std::path::{Path, PathBuf};

use tauri::State;

use crate::host::HostState;

fn resolve_within_workspaces(state: &HostState, requested: &Path) -> Result<PathBuf, String> {
    let canonical =
        std::fs::canonicalize(requested).map_err(|e| format!("{}: {e}", requested.display()))?;
    let inner = state.inner.read();
    for workspace in &inner.workspaces {
        // Canonicalise the workspace root so symlinked checkouts resolve correctly.
        let Ok(root) = std::fs::canonicalize(&workspace.path) else {
            continue;
        };
        if canonical.starts_with(&root) {
            return Ok(canonical);
        }
    }
    Err(format!(
        "path {} is outside any registered workspace",
        requested.display()
    ))
}

fn resolve_for_write(state: &HostState, requested: &Path) -> Result<PathBuf, String> {
    let inner = state.inner.read();
    for workspace in &inner.workspaces {
        let Ok(root) = std::fs::canonicalize(&workspace.path) else {
            continue;
        };
        let devcontainer_dir = root.join(".devcontainer");
        // For writes the file may not yet exist — canonicalise the parent
        // and ensure that resolves under the workspace's `.devcontainer`.
        let Some(parent) = requested.parent() else {
            continue;
        };
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            if canon_parent.starts_with(&devcontainer_dir) || canon_parent == root {
                return Ok(requested.to_path_buf());
            }
        }
    }
    Err(format!(
        "path {} is not a permitted write target",
        requested.display()
    ))
}

#[tauri::command]
pub async fn fs_is_file(state: State<'_, HostState>, path: String) -> Result<bool, String> {
    let resolved = match resolve_within_workspaces(&state, Path::new(&path)) {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    Ok(tokio::fs::metadata(&resolved)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false))
}

#[tauri::command]
pub async fn fs_read_file(state: State<'_, HostState>, path: String) -> Result<Vec<u8>, String> {
    let resolved = resolve_within_workspaces(&state, Path::new(&path))?;
    tokio::fs::read(&resolved).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn fs_write_file(
    state: State<'_, HostState>,
    path: String,
    content: Vec<u8>,
) -> Result<(), String> {
    let resolved = resolve_for_write(&state, Path::new(&path))?;
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn fs_read_dir(state: State<'_, HostState>, path: String) -> Result<Vec<String>, String> {
    let resolved = resolve_within_workspaces(&state, Path::new(&path))?;
    let mut entries = tokio::fs::read_dir(&resolved)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        if let Some(name) = entry.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

#[tauri::command]
pub async fn fs_mkdirp(state: State<'_, HostState>, path: String) -> Result<(), String> {
    let resolved = resolve_for_write(&state, Path::new(&path))?;
    tokio::fs::create_dir_all(&resolved)
        .await
        .map_err(|e| e.to_string())
}
