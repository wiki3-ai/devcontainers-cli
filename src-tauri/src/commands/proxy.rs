//! Internal caching proxy — read-only stats for the UI.
//!
//! Phase 1 only exposes a snapshot. Future phases will add a
//! `proxy_clear_cache` command and per-protocol cache reports.

use tauri::State;

use devcontainer_core::{LifecycleOrchestrator, ProxyStatsSnapshot};

/// Returns `None` when the internal proxy is disabled or its bind
/// failed (e.g. the Apple Containers bridge interface isn't up yet).
/// The UI should render that as "proxy: off".
#[tauri::command]
pub fn proxy_stats(orchestrator: State<'_, LifecycleOrchestrator>) -> Option<ProxyStatsSnapshot> {
    orchestrator.proxy().stats()
}
