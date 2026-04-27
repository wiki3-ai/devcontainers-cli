//! Host layer — config, permissions, persistent state, native menu.
//!
//! Ported in shape from `wiki3-ai/wiki3-app/src-tauri/src/`. Modules:
//!   * [`config`] — app configuration and trusted-origin allowlist.
//!   * [`permissions`] — execution permission model (allow once / always / deny).
//!   * [`menu`] — native macOS menu construction and event routing.
//!   * [`window_state`] — persisted window geometry and per-window flags.
//!
//! [`HostState`] is the top-level singleton stored in Tauri's state map.

pub mod config;
pub mod menu;
pub mod permissions;
pub mod window_state;

use std::path::PathBuf;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use self::config::AppConfig;
use self::permissions::PermissionPolicy;
use self::window_state::WindowState;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("could not determine app data directory")]
    NoDataDir,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Top-level host state. Wrapped in a `RwLock` so individual command
/// handlers can take fine-grained read/write locks without blocking each
/// other.
#[derive(Debug, Default)]
pub struct HostState {
    pub inner: RwLock<HostStateInner>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostStateInner {
    pub config: AppConfig,
    pub policy: PermissionPolicy,
    pub windows: WindowState,
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub selected_runtime: Option<String>,
}

/// A persisted dev container workspace registered in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub path: PathBuf,
    pub display_name: String,
    #[serde(default)]
    pub last_opened_at: Option<String>,
}

impl HostState {
    /// Load persistent state from `<app-data>/state.json` if present.
    pub fn load() -> Result<Self, HostError> {
        let path = state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(&path)?;
        let inner: HostStateInner = serde_json::from_slice(&bytes)?;
        Ok(Self {
            inner: RwLock::new(inner),
        })
    }

    /// Persist current state to disk. Best-effort — caller should log errors.
    pub fn save(&self) -> Result<(), HostError> {
        let path = state_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&*self.inner.read())?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

fn state_path() -> Result<PathBuf, HostError> {
    let dir = dirs::data_dir().ok_or(HostError::NoDataDir)?;
    Ok(dir.join("ai.wiki3.devcontainers").join("state.json"))
}
