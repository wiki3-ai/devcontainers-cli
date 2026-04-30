//! [`EventSink`] — emission seam used by [`crate::devcontainer::lifecycle::LifecycleOrchestrator`].
//!
//! The orchestrator never imports from `tauri`. Each host (the
//! Devcontainers.app binary, the Wiki3 binary, …) provides its own
//! `EventSink` impl that maps `status(...)` / `log(...)` calls onto
//! whatever IPC primitive that host uses. Tests use an in-memory sink.

use crate::container::LogStreamKind;

pub trait EventSink: Send + Sync {
    fn status(
        &self,
        workspace_id: &str,
        state: &str,
        container_id: Option<&str>,
        image_ref: Option<&str>,
        error: Option<&str>,
    );
    fn log(&self, workspace_id: &str, stream: LogStreamKind, line: &str);
}
