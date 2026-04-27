//! Persisted window geometry and per-window flags. Ported in shape from
//! `wiki3-ai/wiki3-app`'s `window_state.rs`. The full restore-on-launch
//! behaviour lands alongside the MVP UI (step 8).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WindowState {
    #[serde(default)]
    pub dashboard: Option<WindowGeometry>,
    #[serde(default)]
    pub by_label: HashMap<String, WindowGeometry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub maximized: bool,
}
