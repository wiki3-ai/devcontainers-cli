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
//!
//! All event emission is funnelled through [`EventSink`] so the orchestrator
//! body can be unit-tested without a real `tauri::AppHandle`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::container::{
    ContainerRuntime, ContainerRuntimeError, ExecOptions, LogStreamKind, RuntimeRegistry,
};
use crate::devcontainer::translate::{to_container_spec, LifecycleCommand, ParsedDevContainer};

/// Reject devcontainer configs that name features the v1 host can't yet
/// honour, so we surface an explicit error instead of silently launching
/// the wrong image. Today: `build.dockerfile` and `dockerComposeFile`
/// are not implemented; users must specify `image`.
fn validate_supported(parsed: &ParsedDevContainer) -> Result<(), LifecycleError> {
    if parsed.image.is_some() {
        return Ok(());
    }
    if parsed.build.is_some() {
        return Err(LifecycleError::Unsupported(
            "`build` (Dockerfile) is not yet implemented in v1; please set an `image` field on the devcontainer.json or wait for build support".into(),
        ));
    }
    if parsed.docker_compose_file.is_some() {
        return Err(LifecycleError::Unsupported(
            "`dockerComposeFile` is not yet implemented in v1; please set an `image` field on the devcontainer.json".into(),
        ));
    }
    Err(LifecycleError::Unsupported(
        "devcontainer.json must specify an `image` field (build/compose support not yet implemented)".into(),
    ))
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("no devcontainer.json submitted for workspace {0}")]
    NoConfig(String),
    #[error("{stage} failed: {source}")]
    Stage {
        stage: &'static str,
        #[source]
        source: ContainerRuntimeError,
    },
    #[error("hook {label} exited with {exit_code}")]
    Hook {
        label: &'static str,
        exit_code: i32,
    },
    #[error("unsupported devcontainer.json: {0}")]
    Unsupported(String),
}

impl From<ContainerRuntimeError> for LifecycleError {
    fn from(source: ContainerRuntimeError) -> Self {
        Self::Stage {
            stage: "runtime",
            source,
        }
    }
}

fn stage<T>(
    stage: &'static str,
    r: Result<T, ContainerRuntimeError>,
) -> Result<T, LifecycleError> {
    r.map_err(|source| LifecycleError::Stage { stage, source })
}

/// Emission seam used by the orchestrator. Production code uses
/// [`TauriSink`]; tests use an in-memory [`CapturingSink`].
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

/// Default sink that forwards events to the running Tauri app.
pub struct TauriSink<'a>(pub &'a AppHandle);

impl EventSink for TauriSink<'_> {
    fn status(
        &self,
        workspace_id: &str,
        state: &str,
        container_id: Option<&str>,
        image_ref: Option<&str>,
        error: Option<&str>,
    ) {
        let _ = self.0.emit(
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

    fn log(&self, workspace_id: &str, stream: LogStreamKind, line: &str) {
        let stream_str = match stream {
            LogStreamKind::Stdout => "stdout",
            LogStreamKind::Stderr => "stderr",
            LogStreamKind::System => "system",
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = self.0.emit(
            "devcontainer://log",
            LogEvent {
                workspace_id,
                stream: stream_str,
                line,
                ts,
            },
        );
    }
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
    last_error: Option<String>,
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
                error: slot.last_error.clone(),
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
        slot.last_error = None;
        if let Some(cid) = container_id {
            slot.container_id = Some(cid.to_string());
        }
        if let Some(img) = image_ref {
            slot.last_image_ref = Some(img.to_string());
        }
    }

    fn record_error(&self, workspace_id: &str, message: &str) {
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.last_state = Some("error");
        slot.last_error = Some(message.to_string());
    }

    /// Public entry point used by `container_up`. Wraps [`Self::up_with_sink`]
    /// with a [`TauriSink`].
    pub async fn up(
        &self,
        app: &AppHandle,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        self.up_with_sink(&TauriSink(app), registry, workspace_id, host_workspace)
            .await
    }

    /// Drive `pull → create → start` and run the post-create/start/attach
    /// lifecycle hooks against the selected runtime. Sink-parameterised so
    /// tests can capture the emitted events without a real Tauri app.
    pub async fn up_with_sink(
        &self,
        sink: &dyn EventSink,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        let lock = self.lock_for(workspace_id);
        let _guard = lock.lock().await;

        let result = self
            .up_inner(sink, registry, workspace_id, host_workspace)
            .await;
        if let Err(err) = &result {
            self.report_failure(sink, workspace_id, "up", err);
        }
        result
    }

    async fn up_inner(
        &self,
        sink: &dyn EventSink,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        let parsed = self.parsed(workspace_id)?;
        let runtime = registry.selected();
        validate_supported(&parsed)?;
        let spec = to_container_spec(&parsed, workspace_id, host_workspace);

        info!(
            workspace = workspace_id,
            runtime = ?runtime.id(),
            image = %spec.image.repository,
            "lifecycle.up starting"
        );

        sink.status(workspace_id, "pulling", None, Some(&spec.image.repository), None);
        sink.log(
            workspace_id,
            LogStreamKind::System,
            &format!("pulling image {}", spec.image.repository),
        );
        debug!(workspace = workspace_id, "stage=pull begin");
        stage("pull", runtime.pull(&spec.image).await)?;
        debug!(workspace = workspace_id, "stage=pull end");

        sink.status(workspace_id, "creating", None, Some(&spec.image.repository), None);
        sink.log(
            workspace_id,
            LogStreamKind::System,
            &format!("creating container {}", spec.name),
        );
        debug!(workspace = workspace_id, "stage=create begin");
        let container_id = stage("create", runtime.create(&spec).await)?;
        debug!(workspace = workspace_id, container = %container_id, "stage=create end");

        self.record_state(
            workspace_id,
            "created",
            Some(&container_id),
            Some(&spec.image.repository),
        );
        sink.status(
            workspace_id,
            "created",
            Some(&container_id),
            Some(&spec.image.repository),
            None,
        );

        sink.log(
            workspace_id,
            LogStreamKind::System,
            &format!("starting container {container_id}"),
        );
        debug!(workspace = workspace_id, "stage=start begin");
        stage("start", runtime.start(&container_id).await)?;
        debug!(workspace = workspace_id, "stage=start end");
        self.record_state(workspace_id, "running", Some(&container_id), None);
        sink.status(
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
                    sink,
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

        info!(workspace = workspace_id, container = %container_id, "lifecycle.up running");
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
        self.stop_with_sink(&TauriSink(app), registry, workspace_id).await
    }

    pub async fn stop_with_sink(
        &self,
        sink: &dyn EventSink,
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
            info!(workspace = workspace_id, container = %cid, "lifecycle.stop");
            if let Err(err) = stage("stop", runtime.stop(&cid).await) {
                self.report_failure(sink, workspace_id, "stop", &err);
                return Err(err);
            }
            self.record_state(workspace_id, "stopped", Some(&cid), None);
            sink.status(workspace_id, "stopped", Some(&cid), None, None);
        } else {
            debug!(workspace = workspace_id, "stop: no container recorded");
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
        self.remove_with_sink(&TauriSink(app), registry, workspace_id).await
    }

    pub async fn remove_with_sink(
        &self,
        sink: &dyn EventSink,
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
            info!(workspace = workspace_id, container = %cid, "lifecycle.remove");
            if let Err(err) = stage("remove", runtime.remove(&cid, true).await) {
                self.report_failure(sink, workspace_id, "remove", &err);
                return Err(err);
            }
            let mut map = self.slots.write();
            if let Some(slot) = map.get_mut(workspace_id) {
                slot.container_id = None;
                slot.last_state = Some("absent");
                slot.last_image_ref = None;
                slot.last_error = None;
            }
            sink.status(workspace_id, "absent", None, None, None);
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
        self.rebuild_with_sink(&TauriSink(app), registry, workspace_id, host_workspace)
            .await
    }

    pub async fn rebuild_with_sink(
        &self,
        sink: &dyn EventSink,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        // `up` and `remove` already grab the per-workspace lock, so call
        // them sequentially without holding it ourselves.
        info!(workspace = workspace_id, "lifecycle.rebuild");
        self.remove_with_sink(sink, registry, workspace_id).await?;
        self.up_with_sink(sink, registry, workspace_id, host_workspace)
            .await
    }

    /// Surface a failed lifecycle stage to logs and the WebView so the user
    /// can see *why*. Without this, an `Err` from a `runtime.*` call only
    /// surfaces as the rejected Tauri command Promise — the `error`
    /// status event is never emitted and the in-app terminal stays silent.
    fn report_failure(
        &self,
        sink: &dyn EventSink,
        workspace_id: &str,
        op: &'static str,
        err: &LifecycleError,
    ) {
        let detail = err.to_string();
        error!(
            workspace = workspace_id,
            op,
            error = %detail,
            "lifecycle operation failed"
        );
        self.record_error(workspace_id, &detail);
        sink.log(
            workspace_id,
            LogStreamKind::Stderr,
            &format!("{op} failed: {detail}"),
        );
        sink.status(workspace_id, "error", None, None, Some(&detail));
    }
}

async fn run_hook(
    sink: &dyn EventSink,
    runtime: &dyn ContainerRuntime,
    workspace_id: &str,
    container_id: &str,
    spec: &crate::container::ContainerSpec,
    label: &'static str,
    cmd: &LifecycleCommand,
) -> Result<(), LifecycleError> {
    let argv = cmd.to_argv();
    if argv.is_empty() {
        return Ok(());
    }
    info!(
        workspace = workspace_id,
        container = container_id,
        hook = label,
        "running lifecycle hook"
    );
    sink.log(
        workspace_id,
        LogStreamKind::System,
        &format!("running {label}: {}", argv.join(" ")),
    );
    let opts = ExecOptions {
        command: argv,
        workdir: spec.workdir.clone(),
        env: spec.env.clone(),
        user: spec.user.clone(),
        tty: false,
    };
    let result = stage("hook", runtime.exec(container_id, &opts).await)?;
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        if !line.is_empty() {
            sink.log(workspace_id, LogStreamKind::Stdout, line);
        }
    }
    for line in String::from_utf8_lossy(&result.stderr).lines() {
        if !line.is_empty() {
            sink.log(workspace_id, LogStreamKind::Stderr, line);
        }
    }
    if result.exit_code != 0 {
        warn!(
            workspace = workspace_id,
            hook = label,
            exit_code = result.exit_code,
            "lifecycle hook exited non-zero"
        );
        return Err(LifecycleError::Hook {
            label,
            exit_code: result.exit_code,
        });
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
    use crate::container::traits::{
        ContainerStatus, ExecResult, LogStream, RuntimeAvailability, RuntimeId,
    };
    use crate::container::{ContainerRuntime, ImageRef, RuntimeRegistry};
    use async_trait::async_trait;
    use parking_lot::Mutex as PlMutex;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    // -------- test fixtures (capturing sink + scriptable fake runtime) --------

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // container_id is captured for future assertions.
    struct CapturedStatus {
        state: String,
        container_id: Option<String>,
        error: Option<String>,
    }

    #[derive(Debug, Clone)]
    struct CapturedLog {
        stream: &'static str,
        line: String,
    }

    #[derive(Default)]
    struct CapturingSink {
        statuses: PlMutex<Vec<CapturedStatus>>,
        logs: PlMutex<Vec<CapturedLog>>,
    }

    impl EventSink for CapturingSink {
        fn status(
            &self,
            _workspace_id: &str,
            state: &str,
            container_id: Option<&str>,
            _image_ref: Option<&str>,
            error: Option<&str>,
        ) {
            self.statuses.lock().push(CapturedStatus {
                state: state.to_string(),
                container_id: container_id.map(str::to_string),
                error: error.map(str::to_string),
            });
        }
        fn log(&self, _workspace_id: &str, stream: LogStreamKind, line: &str) {
            let s = match stream {
                LogStreamKind::Stdout => "stdout",
                LogStreamKind::Stderr => "stderr",
                LogStreamKind::System => "system",
            };
            self.logs.lock().push(CapturedLog {
                stream: s,
                line: line.to_string(),
            });
        }
    }

    /// Per-stage scripted outcome for [`FakeRuntime`].
    #[derive(Default)]
    struct FakeScript {
        pull_err: Option<String>,
        create_err: Option<String>,
        start_err: Option<String>,
    }

    struct FakeRuntime {
        script: PlMutex<FakeScript>,
    }

    impl FakeRuntime {
        fn new(script: FakeScript) -> Self {
            Self {
                script: PlMutex::new(script),
            }
        }
    }

    #[async_trait]
    impl ContainerRuntime for FakeRuntime {
        fn id(&self) -> RuntimeId {
            RuntimeId::AppleContainers
        }
        async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
            Ok(RuntimeAvailability {
                available: true,
                version: Some("fake".into()),
                reason: None,
            })
        }
        async fn pull(&self, _image: &ImageRef) -> Result<(), ContainerRuntimeError> {
            if let Some(e) = self.script.lock().pull_err.take() {
                return Err(ContainerRuntimeError::Backend(e));
            }
            Ok(())
        }
        async fn create(
            &self,
            _spec: &crate::container::ContainerSpec,
        ) -> Result<String, ContainerRuntimeError> {
            if let Some(e) = self.script.lock().create_err.take() {
                return Err(ContainerRuntimeError::Backend(e));
            }
            Ok("fake-cid-1".into())
        }
        async fn start(&self, _container_id: &str) -> Result<(), ContainerRuntimeError> {
            if let Some(e) = self.script.lock().start_err.take() {
                return Err(ContainerRuntimeError::Backend(e));
            }
            Ok(())
        }
        async fn stop(&self, _container_id: &str) -> Result<(), ContainerRuntimeError> {
            Ok(())
        }
        async fn remove(
            &self,
            _container_id: &str,
            _force: bool,
        ) -> Result<(), ContainerRuntimeError> {
            Ok(())
        }
        async fn inspect(
            &self,
            container_id: &str,
        ) -> Result<ContainerStatus, ContainerRuntimeError> {
            Ok(ContainerStatus {
                container_id: container_id.into(),
                state: crate::container::ContainerState::Running,
                image_ref: None,
            })
        }
        async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
            Ok(vec![])
        }
        async fn exec(
            &self,
            _container_id: &str,
            _options: &ExecOptions,
        ) -> Result<ExecResult, ContainerRuntimeError> {
            Ok(ExecResult {
                exit_code: 0,
                stdout: b"hook stdout line\n".to_vec(),
                stderr: vec![],
            })
        }
        async fn logs(
            &self,
            _container_id: &str,
            _options: &crate::container::LogOptions,
        ) -> Result<LogStream, ContainerRuntimeError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }
    }

    fn registry_with(runtime: FakeRuntime) -> RuntimeRegistry {
        RuntimeRegistry::with_single(RuntimeId::AppleContainers, Arc::new(runtime))
    }

    fn parsed_with_image(img: &str) -> ParsedDevContainer {
        ParsedDevContainer {
            image: Some(img.into()),
            ..Default::default()
        }
    }

    // -------- end-to-end: happy path emits the expected event sequence --------

    #[tokio::test]
    async fn up_emits_pulling_creating_running_in_order() {
        let o = LifecycleOrchestrator::new();
        o.set_parsed_config("ws", parsed_with_image("ubuntu:24.04"));
        let registry = registry_with(FakeRuntime::new(FakeScript::default()));
        let sink = CapturingSink::default();

        let status = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect("up should succeed");
        assert_eq!(status.state, "running");

        let states: Vec<String> = sink
            .statuses
            .lock()
            .iter()
            .map(|s| s.state.clone())
            .collect();
        assert_eq!(states, vec!["pulling", "creating", "created", "running"]);

        let log_lines: Vec<String> =
            sink.logs.lock().iter().map(|l| l.line.clone()).collect();
        assert!(
            log_lines.iter().any(|l| l.starts_with("pulling image ubuntu")),
            "missing pull log line; got: {log_lines:?}"
        );
        assert!(
            log_lines.iter().any(|l| l.contains("creating container")),
            "missing create log line; got: {log_lines:?}"
        );
    }

    // -------- failure mode: pull fails -> error status with detail surfaces --------

    #[tokio::test]
    async fn up_pull_failure_surfaces_error_status_and_log() {
        let o = LifecycleOrchestrator::new();
        o.set_parsed_config("ws", parsed_with_image("ubuntu:24.04"));
        let registry = registry_with(FakeRuntime::new(FakeScript {
            pull_err: Some("manifest unknown for ubuntu:24.04".into()),
            ..Default::default()
        }));
        let sink = CapturingSink::default();

        let err = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect_err("up should fail when pull fails");

        // The error returned to the caller carries the stage *and* the
        // backend detail so the Tauri command's `Err(e.to_string())` is
        // useful in the UI.
        let msg = err.to_string();
        assert!(msg.contains("pull"), "missing stage in error: {msg}");
        assert!(
            msg.contains("manifest unknown"),
            "missing backend detail in error: {msg}"
        );

        // The orchestrator must also push an `error` status event and a
        // stderr log line so the in-app terminal/status pill update — this
        // is the regression that gave us "error with no explanation".
        let last = sink.statuses.lock().last().cloned().expect("status emitted");
        assert_eq!(last.state, "error");
        let detail = last.error.expect("error status carries detail");
        assert!(
            detail.contains("manifest unknown"),
            "error detail dropped: {detail}"
        );

        let stderr_lines: Vec<String> = sink
            .logs
            .lock()
            .iter()
            .filter(|l| l.stream == "stderr")
            .map(|l| l.line.clone())
            .collect();
        assert!(
            stderr_lines.iter().any(|l| l.contains("up failed")),
            "missing stderr log line; got: {stderr_lines:?}"
        );

        // And the recorded snapshot now reflects the error so subsequent
        // `container_status` calls expose the same detail.
        let snap = o.snapshot("ws");
        assert_eq!(snap.state, "error");
        assert!(snap
            .error
            .as_deref()
            .unwrap_or("")
            .contains("manifest unknown"));
    }

    #[tokio::test]
    async fn rebuild_runs_remove_then_up_and_recovers_to_running() {
        let o = LifecycleOrchestrator::new();
        o.set_parsed_config("ws", parsed_with_image("ubuntu:24.04"));
        // Pre-seed a stale container id so `remove` actually shells out.
        o.record_state("ws", "stopped", Some("old-cid"), Some("ubuntu:24.04"));
        let registry = registry_with(FakeRuntime::new(FakeScript::default()));
        let sink = CapturingSink::default();

        let status = o
            .rebuild_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect("rebuild should succeed");
        assert_eq!(status.state, "running");
        assert_eq!(status.container_id.as_deref(), Some("fake-cid-1"));

        let states: Vec<String> = sink
            .statuses
            .lock()
            .iter()
            .map(|s| s.state.clone())
            .collect();
        // remove emits "absent" first; then up emits the usual sequence.
        assert_eq!(
            states,
            vec!["absent", "pulling", "creating", "created", "running"]
        );
    }

    // -------- unsupported config: build/dockerfile is rejected explicitly --------

    #[tokio::test]
    async fn up_with_dockerfile_build_reports_unsupported_error() {
        let o = LifecycleOrchestrator::new();
        let parsed = ParsedDevContainer {
            name: Some("JupyterLite Demo".into()),
            build: Some(serde_json::json!({"dockerfile": "Dockerfile"})),
            ..Default::default()
        };
        o.set_parsed_config("ws", parsed);
        let registry = registry_with(FakeRuntime::new(FakeScript::default()));
        let sink = CapturingSink::default();

        let err = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect_err("build-based config must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("build") && msg.contains("not yet implemented"),
            "expected an unsupported-build error, got: {msg}"
        );

        // The error should also surface to the UI (status pill + log line)
        // so the user does not see a silent default-image fallback.
        let states: Vec<String> = sink
            .statuses
            .lock()
            .iter()
            .map(|s| s.state.clone())
            .collect();
        assert!(
            states.contains(&"error".to_string()),
            "expected error status; got: {states:?}"
        );
    }
}
