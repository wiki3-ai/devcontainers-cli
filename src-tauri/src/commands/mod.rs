//! Tauri command surface. Each submodule corresponds to one logical area
//! of the bridge defined in `frontend/src/lib/bridge.ts`.

pub mod fs;
pub mod lifecycle;
pub mod runtime;
pub mod workspace;
