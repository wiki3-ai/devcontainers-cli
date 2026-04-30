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
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ExecOptions, ImageRef, LogChunk,
    LogStreamKind, RuntimeRegistry,
};
use crate::devcontainer::translate::{
    parse_image_ref, to_container_spec, DevContainerBuild, LifecycleCommand, ParsedDevContainer,
};

/// Label key under which the orchestrator stamps the configuration
/// fingerprint on every built image and created container. Reading it
/// back during `up` lets us tell the user the cached artifact is stale
/// without keeping any in-memory state.
pub const LABEL_CONFIG_HASH: &str = "org.devcontainers.config_hash";

/// Reject only what we genuinely cannot run yet. Today that's compose;
/// `image` and `build` (Dockerfile) are both supported.
fn validate_supported(parsed: &ParsedDevContainer) -> Result<(), LifecycleError> {
    if parsed.docker_compose_file.is_some() {
        return Err(LifecycleError::Unsupported(
            "`dockerComposeFile` is not supported \u{2014} this app uses a one-container-per-repo model. Replace the compose file with an `image` or `build` stanza.".into(),
        ));
    }
    if parsed.image.is_none() && parsed.build.is_none() {
        return Err(LifecycleError::Unsupported(
            "devcontainer.json must specify either `image` or `build`".into(),
        ));
    }
    Ok(())
}

/// Coerce an arbitrary slug into a valid OCI image tag fragment:
/// lowercase, `[a-z0-9._-]` only, max 128 chars. Empty/all-bad input
/// collapses to `workspace` so we always produce a runnable tag.
fn sanitize_image_tag(input: &str) -> String {
    let mut out: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with(['-', '.', '_']) {
        out.remove(0);
    }
    while out.ends_with(['-', '.', '_']) {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("workspace");
    }
    if out.len() > 128 {
        out.truncate(128);
    }
    out
}

/// Compute a fingerprint of the inputs that should force a rebuild when
/// they change: the bytes of `devcontainer.json` plus, if the config
/// uses a `build:` stanza, the bytes of the resolved Dockerfile. The
/// hash deliberately ignores files merely referenced by `COPY` in the
/// Dockerfile — BuildKit's own layer cache handles those, and the
/// "Rebuild" button is the escape hatch when it gets it wrong.
///
/// Returns `None` when neither input can be read; callers treat that
/// as "no fingerprint available" and skip both the stamp and the
/// drift check rather than stamp something meaningless.
fn compute_config_hash(parsed: &ParsedDevContainer, dockerfile: Option<&Path>) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut got_input = false;
    if let Some(cfg) = parsed.config_file_path.as_ref() {
        if let Ok(bytes) = std::fs::read(cfg) {
            hasher.update(b"devcontainer.json:");
            hasher.update(&bytes);
            got_input = true;
        }
    }
    if let Some(df) = dockerfile {
        if let Ok(bytes) = std::fs::read(df) {
            hasher.update(b"Dockerfile:");
            hasher.update(&bytes);
            got_input = true;
        }
    }
    if !got_input {
        return None;
    }
    Some(hex::encode(hasher.finalize()))
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
    #[error("hook {label} exited with {exit_code}{stderr_suffix}", stderr_suffix = if stderr_tail.is_empty() { String::new() } else { format!(": {}", stderr_tail) })]
    Hook {
        label: &'static str,
        exit_code: i32,
        stderr_tail: String,
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

fn stage<T>(stage: &'static str, r: Result<T, ContainerRuntimeError>) -> Result<T, LifecycleError> {
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
    /// Host path of the repo this slot represents. Recorded by `up` so
    /// `remove` can re-derive the container name after an app restart
    /// or when no `container_id` was ever recorded.
    host_workspace: Option<std::path::PathBuf>,
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

        // Record the host path so `remove` can re-derive the container
        // name even if `up` fails before `container_id` is recorded.
        {
            let mut map = self.slots.write();
            map.entry(workspace_id.to_string())
                .or_default()
                .host_workspace = Some(host_workspace.to_path_buf());
        }

        let result = self
            .up_inner(sink, registry, workspace_id, host_workspace)
            .await;
        if let Err(err) = &result {
            self.report_failure(sink, workspace_id, "up", err);
        }
        result
    }

    /// Decide whether to pull a pre-built image or build one from a
    /// Dockerfile, returning the resulting [`ImageRef`]. Build output
    /// is streamed to `sink` line-by-line.
    async fn resolve_image(
        &self,
        sink: &dyn EventSink,
        runtime: &dyn ContainerRuntime,
        workspace_id: &str,
        host_workspace: &Path,
        parsed: &ParsedDevContainer,
        config_hash: Option<&str>,
    ) -> Result<ImageRef, LifecycleError> {
        if let Some(build) = parsed.build.as_ref() {
            let mut build_spec =
                self.resolve_build_spec(workspace_id, host_workspace, parsed, build);
            if let Some(h) = config_hash {
                build_spec
                    .labels
                    .insert(LABEL_CONFIG_HASH.to_string(), h.to_string());
            }

            // Check the local image cache: if a previous build already
            // produced an image with the same config-hash label, we can
            // skip the build entirely. The user clicks Rebuild to force
            // a fresh build.
            if let Some(h) = config_hash {
                if let Ok(Some(existing)) = runtime
                    .image_label(&build_spec.tag, LABEL_CONFIG_HASH)
                    .await
                {
                    if existing == h {
                        sink.log(
                            workspace_id,
                            LogStreamKind::System,
                            &format!(
                                "image {} already current (config_hash matches); skipping build",
                                build_spec.tag.repository
                            ),
                        );
                        info!(
                            workspace = workspace_id,
                            tag = %build_spec.tag.repository,
                            "stage=build skipped (cache hit on config_hash)"
                        );
                        return Ok(build_spec.tag);
                    }
                }
            }

            sink.status(
                workspace_id,
                "building",
                None,
                Some(&build_spec.tag.repository),
                None,
            );
            sink.log(
                workspace_id,
                LogStreamKind::System,
                &format!(
                    "building image {} from {}",
                    build_spec.tag.repository,
                    build_spec.dockerfile.display()
                ),
            );
            info!(
                workspace = workspace_id,
                tag = %build_spec.tag.repository,
                dockerfile = %build_spec.dockerfile.display(),
                context = %build_spec.context_dir.display(),
                "stage=build begin"
            );

            // Bounded channel so a slow consumer can't grow memory.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<LogChunk>(256);

            // Forward chunks to the sink *as they arrive* (don't buffer
            // until the build completes — we want live progress in the
            // dashboard log pane). We can't `tokio::spawn` the pump
            // because `sink: &dyn EventSink` is not 'static, so we run
            // it inline via `tokio::join!` on the same task.
            let build_fut = runtime.build(&build_spec, Some(tx));
            let pump_fut = async {
                while let Some(chunk) = rx.recv().await {
                    sink.log(workspace_id, chunk.stream, &chunk.line);
                }
            };
            let (result, ()) = tokio::join!(build_fut, pump_fut);
            let image_ref = stage("build", result)?;
            info!(workspace = workspace_id, image = %image_ref.repository, "stage=build done");
            Ok(image_ref)
        } else {
            let image_str = parsed
                .image
                .as_deref()
                .expect("validate_supported guarantees image or build");
            let image_ref = parse_image_ref(image_str);
            // Skip the pull when the image is already present locally;
            // pulled images don't carry our config_hash label so we
            // rely on simple presence here. Network-side updates
            // (registry tag moved) are handled by Rebuild.
            if matches!(runtime.image_exists(&image_ref).await, Ok(true)) {
                sink.log(
                    workspace_id,
                    LogStreamKind::System,
                    &format!(
                        "image {} already present locally; skipping pull",
                        image_ref.repository
                    ),
                );
                info!(workspace = workspace_id, image = %image_ref.repository, "stage=pull skipped (already local)");
                return Ok(image_ref);
            }
            sink.status(
                workspace_id,
                "pulling",
                None,
                Some(&image_ref.repository),
                None,
            );
            sink.log(
                workspace_id,
                LogStreamKind::System,
                &format!("pulling image {}", image_ref.repository),
            );
            info!(workspace = workspace_id, image = %image_ref.repository, "stage=pull begin");
            stage("pull", runtime.pull(&image_ref).await)?;
            info!(workspace = workspace_id, image = %image_ref.repository, "stage=pull done");
            Ok(image_ref)
        }
    }

    /// Resolve the parsed build stanza into an absolute [`BuildSpec`].
    /// Per the upstream spec, `dockerfile` and `context` are relative to
    /// the `.devcontainer/` folder (the parent of `devcontainer.json`).
    fn resolve_build_spec(
        &self,
        workspace_id: &str,
        host_workspace: &Path,
        parsed: &ParsedDevContainer,
        build: &DevContainerBuild,
    ) -> BuildSpec {
        let cfg_dir: std::path::PathBuf = parsed
            .config_file_path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| host_workspace.join(".devcontainer"));

        let dockerfile = cfg_dir.join(build.dockerfile.as_deref().unwrap_or("Dockerfile"));
        let context_dir = cfg_dir.join(build.context.as_deref().unwrap_or("."));

        let workspace_slug = host_workspace
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(workspace_id);
        let tag = parse_image_ref(&format!(
            "devcontainer-{}:latest",
            sanitize_image_tag(workspace_slug)
        ));

        BuildSpec {
            tag,
            context_dir,
            dockerfile,
            build_args: build.args.clone(),
            target: build.target.clone(),
            labels: HashMap::new(),
        }
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
        ensure_runtime_ready(sink, runtime.as_ref(), workspace_id).await?;

        // Compute the config-hash up front so we can both gate
        // unnecessary rebuilds and stamp it on the container we end up
        // with. Only `build:` configs use the Dockerfile component.
        let dockerfile_path: Option<std::path::PathBuf> = parsed.build.as_ref().map(|b| {
            let cfg_dir: std::path::PathBuf = parsed
                .config_file_path
                .as_ref()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| host_workspace.join(".devcontainer"));
            cfg_dir.join(b.dockerfile.as_deref().unwrap_or("Dockerfile"))
        });
        let config_hash = compute_config_hash(&parsed, dockerfile_path.as_deref());

        // Resolve the image: either pull a pre-built one, or build from a
        // Dockerfile. The result is the ImageRef we hand to `create`.
        let image_ref = self
            .resolve_image(
                sink,
                runtime.as_ref(),
                workspace_id,
                host_workspace,
                &parsed,
                config_hash.as_deref(),
            )
            .await?;
        let mut spec = to_container_spec(&parsed, image_ref.clone(), workspace_id, host_workspace);
        if let Some(h) = config_hash.as_deref() {
            spec.labels
                .insert(LABEL_CONFIG_HASH.to_string(), h.to_string());
        }

        info!(
            workspace = workspace_id,
            runtime = ?runtime.id(),
            image = %spec.image.repository,
            "lifecycle.up starting"
        );

        sink.status(
            workspace_id,
            "creating",
            None,
            Some(&spec.image.repository),
            None,
        );
        sink.log(
            workspace_id,
            LogStreamKind::System,
            &format!("creating container {}", spec.name),
        );
        debug!(workspace = workspace_id, "stage=create begin");
        let container_id = match runtime.create(&spec).await {
            Ok(id) => id,
            // Apple's `container` CLI returns: "failed to create container
            // (cause: \"exists: \"container already exists: NAME\"\")".
            // Other backends use varied phrasing for the same condition.
            // We treat "already exists" as recoverable: adopt the existing
            // container by its configured name and continue. The user can
            // hit Rebuild for a fresh one.
            Err(ContainerRuntimeError::Backend(msg)) if is_already_exists(&msg) => {
                warn!(
                    workspace = workspace_id,
                    name = %spec.name,
                    "container already exists; adopting by name"
                );
                sink.log(
                    workspace_id,
                    LogStreamKind::System,
                    &format!(
                        "container `{}` already exists; adopting it (use Rebuild for a fresh container)",
                        spec.name
                    ),
                );
                spec.name.clone()
            }
            Err(err) => return Err(stage::<()>("create", Err(err)).unwrap_err()),
        };
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
        self.stop_with_sink(&TauriSink(app), registry, workspace_id)
            .await
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
            ensure_runtime_ready(sink, runtime.as_ref(), workspace_id).await?;
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
        self.remove_with_sink(&TauriSink(app), registry, workspace_id)
            .await
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
        // Prefer the recorded id, but if we don't have one (e.g. the app
        // was restarted, or `up` failed before recording it) fall back to
        // the container name we *would have* used. This is what the
        // Remove button needs to work after a partial/failed Up.
        let target = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.container_id.clone())
            .or_else(|| {
                let map = self.slots.read();
                let slot = map.get(workspace_id)?;
                let host = slot.host_workspace.as_ref()?;
                Some(derive_container_name(workspace_id, host))
            });
        if let Some(cid) = target {
            ensure_runtime_ready(sink, runtime.as_ref(), workspace_id).await?;
            info!(workspace = workspace_id, container = %cid, "lifecycle.remove");
            match runtime.remove(&cid, true).await {
                Ok(()) => {}
                // Treat "not found" as success — the desired end state
                // (no container) is already true.
                Err(ContainerRuntimeError::Backend(msg)) if is_not_found(&msg) => {
                    info!(
                        workspace = workspace_id,
                        container = %cid,
                        "remove: container not found; treating as success"
                    );
                    sink.log(
                        workspace_id,
                        LogStreamKind::System,
                        &format!("container `{cid}` not found (already removed)"),
                    );
                }
                Err(err) => {
                    let lerr = stage::<()>("remove", Err(err)).unwrap_err();
                    self.report_failure(sink, workspace_id, "remove", &lerr);
                    return Err(lerr);
                }
            }
            let mut map = self.slots.write();
            if let Some(slot) = map.get_mut(workspace_id) {
                slot.container_id = None;
                slot.last_state = Some("absent");
                slot.last_image_ref = None;
                slot.last_error = None;
            }
            sink.status(workspace_id, "absent", None, None, None);
        } else {
            debug!(
                workspace = workspace_id,
                "remove: no container or parsed config"
            );
            self.record_state(workspace_id, "absent", None, None);
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
        // Preserve any container linkage we've already recorded so the
        // dashboard keeps showing the repo↔container relationship even
        // when a hook (postCreateCommand, etc) fails after the container
        // has been created or adopted.
        let (cid, image) = {
            let map = self.slots.read();
            map.get(workspace_id)
                .map(|s| (s.container_id.clone(), s.last_image_ref.clone()))
                .unwrap_or((None, None))
        };
        sink.status(
            workspace_id,
            "error",
            cid.as_deref(),
            image.as_deref(),
            Some(&detail),
        );
    }
}

/// Detect a backend "container already exists" error message regardless
/// of which CLI produced it. Apple `container` says
/// `exists: "container already exists: NAME"`; Docker/Podman wording
/// includes phrases like `is already in use` or `already exists`.
fn is_already_exists(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("already exists") || m.contains("is already in use")
}

/// Detect a backend "no such container" message. Apple says
/// `not found: NAME`; Docker says `No such container`.
fn is_not_found(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("not found") || m.contains("no such container")
}

/// Re-derive the container name we *would have* used for `up`, so the
/// Remove button can clean up after a failed/restarted Up that never
/// got to record a container_id.
fn derive_container_name(workspace_id: &str, host_workspace: &std::path::Path) -> String {
    use crate::devcontainer::translate::{derive_name_from_path, sanitize_entity_name};
    let raw = derive_name_from_path(host_workspace, workspace_id);
    sanitize_entity_name(&raw).unwrap_or_else(|| format!("devcontainer-{workspace_id}"))
}

/// Make sure the backend's daemon/services are up, surfacing the
/// optional "starting…" message via the event sink so the dashboard
/// shows progress on cold starts. Idempotent — Apple's runtime caches
/// the result and short-circuits subsequent calls.
async fn ensure_runtime_ready(
    sink: &dyn EventSink,
    runtime: &dyn ContainerRuntime,
    workspace_id: &str,
) -> Result<(), LifecycleError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogChunk>(8);
    let pump = async {
        while let Some(chunk) = rx.recv().await {
            sink.log(workspace_id, chunk.stream, &chunk.line);
        }
    };
    let work = runtime.ensure_system_running(Some(tx));
    let (result, ()) = tokio::join!(work, pump);
    stage("ensure_system_running", result)?;
    Ok(())
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
    let stdout_text = String::from_utf8_lossy(&result.stdout);
    let stderr_text = String::from_utf8_lossy(&result.stderr);
    for line in stdout_text.lines() {
        if !line.is_empty() {
            sink.log(workspace_id, LogStreamKind::Stdout, line);
        }
    }
    for line in stderr_text.lines() {
        if !line.is_empty() {
            sink.log(workspace_id, LogStreamKind::Stderr, line);
        }
    }
    if result.exit_code != 0 {
        // Keep the tail of stderr so the user sees what failed without
        // having to open the log pane. 1KiB is plenty for a one-line
        // "command not found" / "No such file" diagnostic.
        let mut tail = stderr_text.trim().to_string();
        if tail.len() > 1024 {
            let start = tail.len() - 1024;
            tail = format!("…{}", &tail[start..]);
        }
        warn!(
            workspace = workspace_id,
            hook = label,
            exit_code = result.exit_code,
            stderr = %tail,
            "lifecycle hook exited non-zero"
        );
        return Err(LifecycleError::Hook {
            label,
            exit_code: result.exit_code,
            stderr_tail: tail,
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
        build_err: Option<String>,
        create_err: Option<String>,
        start_err: Option<String>,
        /// Lines emitted on the build log channel before the build
        /// returns. Each is sent as `(stream, line)`.
        build_log: Vec<(LogStreamKind, String)>,
    }

    struct FakeRuntime {
        script: PlMutex<FakeScript>,
        build_calls: PlMutex<Vec<crate::container::BuildSpec>>,
    }

    impl FakeRuntime {
        fn new(script: FakeScript) -> Self {
            Self {
                script: PlMutex::new(script),
                build_calls: PlMutex::new(Vec::new()),
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
        async fn build(
            &self,
            spec: &crate::container::BuildSpec,
            log_sink: Option<tokio::sync::mpsc::Sender<LogChunk>>,
        ) -> Result<ImageRef, ContainerRuntimeError> {
            self.build_calls.lock().push(spec.clone());
            // Drain any scripted log lines into the sink, mirroring how
            // the real backend forwards `container build` output.
            let lines: Vec<(LogStreamKind, String)> =
                std::mem::take(&mut self.script.lock().build_log);
            if let Some(tx) = log_sink {
                for (stream, line) in lines {
                    let _ = tx.send(LogChunk { stream, line }).await;
                }
            }
            if let Some(e) = self.script.lock().build_err.take() {
                return Err(ContainerRuntimeError::Backend(e));
            }
            Ok(spec.tag.clone())
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
                host_mounts: Vec::new(),
                labels: HashMap::new(),
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

        let log_lines: Vec<String> = sink.logs.lock().iter().map(|l| l.line.clone()).collect();
        assert!(
            log_lines
                .iter()
                .any(|l| l.starts_with("pulling image ubuntu")),
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
        let last = sink
            .statuses
            .lock()
            .last()
            .cloned()
            .expect("status emitted");
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

    // -------- build path: dockerfile-based config invokes runtime.build --------

    #[tokio::test]
    async fn up_with_dockerfile_build_invokes_runtime_build_and_emits_building_status() {
        let o = LifecycleOrchestrator::new();
        let parsed = ParsedDevContainer {
            name: Some("JupyterLite Demo".into()),
            build: Some(DevContainerBuild {
                dockerfile: Some("Dockerfile".into()),
                context: Some("..".into()),
                ..Default::default()
            }),
            config_file_path: Some(PathBuf::from(
                "/tmp/take-two/.devcontainer/devcontainer.json",
            )),
            ..Default::default()
        };
        o.set_parsed_config("ws", parsed);

        let runtime = Arc::new(FakeRuntime::new(FakeScript {
            build_log: vec![
                (LogStreamKind::Stdout, "step 1/2: FROM python:3.13".into()),
                (LogStreamKind::Stdout, "step 2/2: RUN apt-get update".into()),
            ],
            ..Default::default()
        }));
        let registry = RuntimeRegistry::with_single(
            RuntimeId::AppleContainers,
            runtime.clone() as Arc<dyn ContainerRuntime>,
        );
        let sink = CapturingSink::default();

        let status = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/take-two"))
            .await
            .expect("up should succeed for build-based config");
        assert_eq!(status.state, "running");

        // The build was invoked with the right resolved paths.
        let calls = runtime.build_calls.lock().clone();
        assert_eq!(calls.len(), 1, "expected one build call");
        let bs = &calls[0];
        assert_eq!(
            bs.dockerfile,
            PathBuf::from("/tmp/take-two/.devcontainer/Dockerfile"),
            "dockerfile resolved relative to .devcontainer/"
        );
        assert_eq!(
            bs.context_dir,
            PathBuf::from("/tmp/take-two/.devcontainer/.."),
            "context resolved relative to .devcontainer/"
        );
        assert!(
            bs.tag.repository.starts_with("devcontainer-take-two"),
            "synthesised tag should be workspace-scoped, got: {}",
            bs.tag.repository
        );

        // Status sequence includes `building` (not `pulling`).
        let states: Vec<String> = sink
            .statuses
            .lock()
            .iter()
            .map(|s| s.state.clone())
            .collect();
        assert_eq!(states, vec!["building", "creating", "created", "running"]);

        // Build log lines surfaced to the dashboard.
        let log_lines: Vec<String> = sink.logs.lock().iter().map(|l| l.line.clone()).collect();
        assert!(
            log_lines
                .iter()
                .any(|l| l.contains("step 1/2: FROM python")),
            "expected build stdout in sink; got: {log_lines:?}"
        );
    }

    // -------- failure mode: build fails -> error status with stderr surfaces --------

    #[tokio::test]
    async fn up_with_build_failure_surfaces_error_status_and_log() {
        let o = LifecycleOrchestrator::new();
        let parsed = ParsedDevContainer {
            build: Some(DevContainerBuild::default()),
            config_file_path: Some(PathBuf::from("/tmp/ws/.devcontainer/devcontainer.json")),
            ..Default::default()
        };
        o.set_parsed_config("ws", parsed);

        let registry = registry_with(FakeRuntime::new(FakeScript {
            build_err: Some("Dockerfile syntax error on line 3".into()),
            ..Default::default()
        }));
        let sink = CapturingSink::default();

        let err = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect_err("build error must propagate");
        let msg = err.to_string();
        assert!(
            msg.contains("build failed") && msg.contains("Dockerfile syntax error"),
            "expected detailed build error, got: {msg}"
        );
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

    // -------- compose is still rejected explicitly (one-container-per-repo) --------

    #[tokio::test]
    async fn up_with_compose_config_reports_unsupported_error() {
        let o = LifecycleOrchestrator::new();
        let parsed = ParsedDevContainer {
            docker_compose_file: Some(serde_json::json!("docker-compose.yml")),
            ..Default::default()
        };
        o.set_parsed_config("ws", parsed);
        let registry = registry_with(FakeRuntime::new(FakeScript::default()));
        let sink = CapturingSink::default();

        let err = o
            .up_with_sink(&sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect_err("compose config must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("dockerComposeFile") && msg.contains("one-container-per-repo"),
            "expected compose-rejection error, got: {msg}"
        );
    }

    // -------- config-hash --------

    #[test]
    fn compute_config_hash_changes_with_devcontainer_json() {
        use std::io::Write;
        let dir = tempdir();
        let cfg = dir.join("devcontainer.json");
        std::fs::File::create(&cfg)
            .unwrap()
            .write_all(b"{\"image\":\"a\"}")
            .unwrap();
        let parsed = ParsedDevContainer {
            image: Some("a".into()),
            config_file_path: Some(cfg.clone()),
            ..Default::default()
        };
        let h1 = compute_config_hash(&parsed, None).expect("hash");

        std::fs::File::create(&cfg)
            .unwrap()
            .write_all(b"{\"image\":\"b\"}")
            .unwrap();
        let h2 = compute_config_hash(&parsed, None).expect("hash");
        assert_ne!(h1, h2, "edits to devcontainer.json must change the hash");
    }

    #[test]
    fn compute_config_hash_changes_with_dockerfile() {
        use std::io::Write;
        let dir = tempdir();
        let cfg = dir.join("devcontainer.json");
        let df = dir.join("Dockerfile");
        std::fs::File::create(&cfg).unwrap().write_all(b"{}").unwrap();
        std::fs::File::create(&df)
            .unwrap()
            .write_all(b"FROM alpine:3.19\n")
            .unwrap();
        let parsed = ParsedDevContainer {
            config_file_path: Some(cfg),
            ..Default::default()
        };
        let h1 = compute_config_hash(&parsed, Some(&df)).unwrap();
        std::fs::File::create(&df)
            .unwrap()
            .write_all(b"FROM alpine:3.20\n")
            .unwrap();
        let h2 = compute_config_hash(&parsed, Some(&df)).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn compute_config_hash_returns_none_when_no_files_readable() {
        let parsed = ParsedDevContainer::default();
        assert!(compute_config_hash(&parsed, None).is_none());
    }

    /// Per-test scratch directory under the OS tempdir. We avoid the
    /// `tempfile` crate to keep the dev dependencies minimal; cleanup
    /// is best-effort and irrelevant for these tiny fixtures.
    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "devc-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
