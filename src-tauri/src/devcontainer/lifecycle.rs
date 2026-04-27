//! Lifecycle orchestrator.
//!
//! Translates a [`ParsedDevContainer`] (posted from the WebView spec slice)
//! into a runtime-agnostic [`crate::container::ContainerSpec`] and drives
//! the selected [`crate::container::ContainerRuntime`] through pull → create
//! → start → lifecycle-hook execution. Per-workspace state is keyed by
//! `workspace_id`:
//!
//! * the parsed `devcontainer.json` (set by `submit_parsed_devcontainer`),
//! * the active `container_id` once `container_up` has run,
//! * a tokio `Mutex` so concurrent commands on the same workspace serialise.
//!
//! Status changes and hook output are emitted to the WebView as
//! `devcontainer://status` and `devcontainer://log` Tauri events. The
//! lifecycle commands in `crate::commands::lifecycle` are thin wrappers
//! around the methods on this struct.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::container::{
    ContainerRuntime, ContainerRuntimeError, ExecOptions, LogStreamKind, RuntimeRegistry,
};
use crate::devcontainer::translate::{to_container_spec, LifecycleCommand, ParsedDevContainer};

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("no devcontainer.json submitted for workspace {0}")]
    NoConfig(String),
    #[error("runtime error: {0}")]
    Runtime(#[from] ContainerRuntimeError),
}

/// Snapshot of a workspace's container state as exposed to the WebView.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleStatus {
    pub workspace_id: String,
    pub state: &'static str,
    pub container_id: Option<String>,
    pub image_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Default)]
struct WorkspaceSlot {
    parsed: Option<ParsedDevContainer>,
    container_id: Option<String>,
    last_image_ref: Option<String>,
    last_state: Option<&'static str>,
}

#[derive(Default)]
pub struct LifecycleOrchestrator {
    /// Per-workspace persistent fields (parsed config, current container id,
    /// last status) read/written under a short-lived `parking_lot::RwLock`.
    slots: RwLock<HashMap<String, WorkspaceSlot>>,
    /// Per-workspace tokio mutex serialising mutating commands. Held across
    /// awaits so we use `tokio::sync::Mutex` rather than `parking_lot`.
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for LifecycleOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleOrchestrator")
            .field("workspaces", &self.slots.read().len())
            .finish()
    }
}

impl LifecycleOrchestrator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the parsed config for `workspace_id`. Replaces any prior value.
    pub fn set_parsed_config(&self, workspace_id: &str, parsed: ParsedDevContainer) {
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.parsed = Some(parsed);
    }

    /// Read-only snapshot helper used by `container_status` when no
    /// runtime call is needed.
    pub fn snapshot(&self, workspace_id: &str) -> LifecycleStatus {
        let map = self.slots.read();
        if let Some(slot) = map.get(workspace_id) {
            return LifecycleStatus {
                workspace_id: workspace_id.to_string(),
                state: slot.last_state.unwrap_or("absent"),
                container_id: slot.container_id.clone(),
                image_ref: slot.last_image_ref.clone(),
                error: None,
            };
        }
        LifecycleStatus {
            workspace_id: workspace_id.to_string(),
            state: "absent",
            container_id: None,
            image_ref: None,
            error: None,
        }
    }

    fn lock_for(&self, workspace_id: &str) -> Arc<Mutex<()>> {
        let mut map = self.locks.write();
        map.entry(workspace_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn parsed(&self, workspace_id: &str) -> Result<ParsedDevContainer, LifecycleError> {
        self.slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.parsed.clone())
            .ok_or_else(|| LifecycleError::NoConfig(workspace_id.to_string()))
    }

    fn record_state(
        &self,
        workspace_id: &str,
        state: &'static str,
        container_id: Option<&str>,
        image_ref: Option<&str>,
    ) {
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.last_state = Some(state);
        if let Some(cid) = container_id {
            slot.container_id = Some(cid.to_string());
        }
        if let Some(img) = image_ref {
            slot.last_image_ref = Some(img.to_string());
        }
    }

    /// Drive `pull → create → start` and run the post-create/start/attach
    /// lifecycle hooks against the selected runtime.
    pub async fn up(
        &self,
        app: &AppHandle,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        let lock = self.lock_for(workspace_id);
        let _guard = lock.lock().await;

        let parsed = self.parsed(workspace_id)?;
        let runtime = registry.selected();
        let spec = to_container_spec(&parsed, workspace_id, host_workspace);

        emit_status(app, workspace_id, "pulling", None, None, None);
        emit_log(
            app,
            workspace_id,
            LogStreamKind::System,
            format!("pulling image {}", spec.image.repository),
        );
        runtime.pull(&spec.image).await?;

        emit_status(app, workspace_id, "creating", None, None, None);
        let container_id = runtime.create(&spec).await?;
        self.record_state(
            workspace_id,
            "created",
            Some(&container_id),
            Some(&spec.image.repository),
        );
        emit_status(
            app,
            workspace_id,
            "created",
            Some(&container_id),
            Some(&spec.image.repository),
            None,
        );

        runtime.start(&container_id).await?;
        self.record_state(workspace_id, "running", Some(&container_id), None);
        emit_status(
            app,
            workspace_id,
            "running",
            Some(&container_id),
            Some(&spec.image.repository),
            None,
        );

        // Lifecycle hooks. `initializeCommand` runs on the host (intentionally
        // not implemented in the MVP — host-side execution is gated on the
        // permission model that lands in Phase 2). The remaining hooks run
        // inside the container via `runtime.exec`.
        for (label, hook) in [
            ("onCreateCommand", parsed.on_create_command.as_ref()),
            (
                "updateContentCommand",
                parsed.update_content_command.as_ref(),
            ),
            ("postCreateCommand", parsed.post_create_command.as_ref()),
            ("postStartCommand", parsed.post_start_command.as_ref()),
            ("postAttachCommand", parsed.post_attach_command.as_ref()),
        ] {
            if let Some(cmd) = hook {
                run_hook(
                    app,
                    runtime.as_ref(),
                    workspace_id,
                    &container_id,
                    &spec,
                    label,
                    cmd,
                )
                .await?;
            }
        }

        Ok(LifecycleStatus {
            workspace_id: workspace_id.to_string(),
            state: "running",
            container_id: Some(container_id),
            image_ref: Some(spec.image.repository),
            error: None,
        })
    }

    pub async fn stop(
        &self,
        app: &AppHandle,
        registry: &RuntimeRegistry,
        workspace_id: &str,
    ) -> Result<LifecycleStatus, LifecycleError> {
        let lock = self.lock_for(workspace_id);
        let _guard = lock.lock().await;

        let runtime = registry.selected();
        let cid = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.container_id.clone());
        if let Some(cid) = cid {
            runtime.stop(&cid).await?;
            self.record_state(workspace_id, "stopped", Some(&cid), None);
            emit_status(app, workspace_id, "stopped", Some(&cid), None, None);
        } else {
            self.record_state(workspace_id, "absent", None, None);
        }
        Ok(self.snapshot(workspace_id))
    }

    pub async fn remove(
        &self,
        app: &AppHandle,
        registry: &RuntimeRegistry,
        workspace_id: &str,
    ) -> Result<LifecycleStatus, LifecycleError> {
        let lock = self.lock_for(workspace_id);
        let _guard = lock.lock().await;

        let runtime = registry.selected();
        let cid = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.container_id.clone());
        if let Some(cid) = cid {
            // Force in case it is still running; `runtime.remove` is idempotent.
            runtime.remove(&cid, true).await?;
            let mut map = self.slots.write();
            if let Some(slot) = map.get_mut(workspace_id) {
                slot.container_id = None;
                slot.last_state = Some("absent");
                slot.last_image_ref = None;
            }
            emit_status(app, workspace_id, "absent", None, None, None);
        }
        Ok(self.snapshot(workspace_id))
    }

    pub async fn rebuild(
        &self,
        app: &AppHandle,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        // `up` and `remove` already grab the per-workspace lock, so call
        // them sequentially without holding it ourselves.
        self.remove(app, registry, workspace_id).await?;
        self.up(app, registry, workspace_id, host_workspace).await
    }
}

async fn run_hook(
    app: &AppHandle,
    runtime: &dyn ContainerRuntime,
    workspace_id: &str,
    container_id: &str,
    spec: &crate::container::ContainerSpec,
    label: &str,
    cmd: &LifecycleCommand,
) -> Result<(), LifecycleError> {
    let argv = cmd.to_argv();
    if argv.is_empty() {
        return Ok(());
    }
    emit_log(
        app,
        workspace_id,
        LogStreamKind::System,
        format!("running {label}: {}", argv.join(" ")),
    );
    let opts = ExecOptions {
        command: argv,
        workdir: spec.workdir.clone(),
        env: spec.env.clone(),
        user: spec.user.clone(),
        tty: false,
    };
    let result = runtime.exec(container_id, &opts).await?;
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        if !line.is_empty() {
            emit_log(app, workspace_id, LogStreamKind::Stdout, line.to_string());
        }
    }
    for line in String::from_utf8_lossy(&result.stderr).lines() {
        if !line.is_empty() {
            emit_log(app, workspace_id, LogStreamKind::Stderr, line.to_string());
        }
    }
    if result.exit_code != 0 {
        return Err(LifecycleError::Runtime(ContainerRuntimeError::Backend(
            format!("{label} exited with {}", result.exit_code),
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusEvent<'a> {
    workspace_id: &'a str,
    state: &'a str,
    container_id: Option<&'a str>,
    image_ref: Option<&'a str>,
    error: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEvent<'a> {
    workspace_id: &'a str,
    stream: &'a str,
    line: &'a str,
    ts: u128,
}

fn emit_status(
    app: &AppHandle,
    workspace_id: &str,
    state: &str,
    container_id: Option<&str>,
    image_ref: Option<&str>,
    error: Option<&str>,
) {
    let _ = app.emit(
        "devcontainer://status",
        StatusEvent {
            workspace_id,
            state,
            container_id,
            image_ref,
            error,
        },
    );
}

fn emit_log(app: &AppHandle, workspace_id: &str, stream: LogStreamKind, line: String) {
    let stream = match stream {
        LogStreamKind::Stdout => "stdout",
        LogStreamKind::Stderr => "stderr",
        LogStreamKind::System => "system",
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = app.emit(
        "devcontainer://log",
        LogEvent {
            workspace_id,
            stream,
            line: &line,
            ts,
        },
    );
}

/// `devcontainer.json` lifecycle hook identifier. Re-exported from the
/// orchestrator so command handlers can use it without importing
/// `translate.rs` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleHook {
    InitializeCommand,
    OnCreateCommand,
    UpdateContentCommand,
    PostCreateCommand,
    PostStartCommand,
    PostAttachCommand,
}

impl LifecycleHook {
    /// Whether the hook runs on the *host* (true) or inside the container.
    pub fn runs_on_host(self) -> bool {
        matches!(self, Self::InitializeCommand)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_command_argv_shapes() {
        let s = LifecycleCommand::Single("echo hi && ls".into());
        assert_eq!(s.to_argv(), vec!["/bin/sh", "-c", "echo hi && ls"]);
        let m = LifecycleCommand::Multiple(vec!["true".into(), "yes".into()]);
        assert_eq!(m.to_argv(), vec!["true", "yes"]);
    }

    #[test]
    fn snapshot_is_absent_for_unknown_workspaces() {
        let o = LifecycleOrchestrator::new();
        let s = o.snapshot("missing");
        assert_eq!(s.state, "absent");
        assert!(s.container_id.is_none());
    }

    #[test]
    fn set_parsed_config_persists_across_calls() {
        let o = LifecycleOrchestrator::new();
        let p = ParsedDevContainer {
            image: Some("ubuntu".into()),
            ..Default::default()
        };
        o.set_parsed_config("ws-1", p);
        assert!(o.parsed("ws-1").is_ok());
        assert!(matches!(o.parsed("ws-2"), Err(LifecycleError::NoConfig(_))));
    }

    #[test]
    fn record_state_overwrites_last_state() {
        let o = LifecycleOrchestrator::new();
        o.record_state("ws", "creating", Some("cid-1"), Some("ubuntu"));
        let s = o.snapshot("ws");
        assert_eq!(s.state, "creating");
        assert_eq!(s.container_id.as_deref(), Some("cid-1"));
        o.record_state("ws", "running", None, None);
        assert_eq!(o.snapshot("ws").state, "running");
        assert_eq!(o.snapshot("ws").container_id.as_deref(), Some("cid-1"));
    }
}
