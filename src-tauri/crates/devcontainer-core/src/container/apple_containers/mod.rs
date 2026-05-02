//! Apple Containers backend.
//!
//! v1 drives the `container` CLI bundled with macOS 26+. Where a Swift/C
//! programmatic API exists in a future macOS release, the implementation
//! should switch to it; until then we shell out to `container …` and parse
//! the JSON output.
//!
//! The exact subcommand surface of `container` is captured in
//! [`cli::ContainerCli`] so it can be unit-tested without the binary
//! installed and replaced with a fake in `apple-containers-live` integration
//! tests.

use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::traits::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerState,
    ContainerStatus, ExecOptions, ExecResult, ImageRef, LogChunk, LogOptions, LogStream,
    LogStreamKind, MountKind, RuntimeAvailability, RuntimeId,
};

mod cli;
pub mod cli_helpers;
#[cfg(test)]
mod tests;

pub use cli::ContainerCli;
pub use cli_helpers::{
    detect, ensure_service_running, find_container_by_mount_source, inspect_container_ipv4,
    is_service_running, list_running_container_names, probe_with_dirs, stop_container_by_name,
    stop_service, AppleContainerStatus,
};

/// Apple Containers backend. The binary name (`container` by default) is
/// configurable via [`AppleContainersRuntime::with_binary`] so the live
/// integration tests can point at a fake on `$PATH`.
#[derive(Debug)]
pub struct AppleContainersRuntime {
    cli: ContainerCli,
    /// Set once `ensure_system_running` confirms the daemon is up. Skips
    /// the `container system status` round-trip on subsequent calls in
    /// the same process. We never reset this — if the daemon dies mid-
    /// session the next CLI call surfaces the failure on its own.
    system_ready: AtomicBool,
}

impl Clone for AppleContainersRuntime {
    fn clone(&self) -> Self {
        Self {
            cli: self.cli.clone(),
            system_ready: AtomicBool::new(self.system_ready.load(Ordering::Relaxed)),
        }
    }
}

impl Default for AppleContainersRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleContainersRuntime {
    /// Build a runtime configured with the canonical `container`
    /// binary. We probe the standard install locations first
    /// (`/usr/local/bin/container` per Apple's installer, then
    /// `/opt/homebrew/bin/container`), falling back to bare
    /// `"container"` for `PATH` lookup.
    ///
    /// The probe matters for GUI apps launched from Finder/Launchpad:
    /// launchd-spawned processes inherit a minimal `PATH`
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`) that does **not** include
    /// `/usr/local/bin`, so `Command::new("container")` would fail
    /// with `No such file or directory` even though `container --version`
    /// works fine in a developer shell. Resolving to an absolute path
    /// here sidesteps that.
    pub fn new() -> Self {
        let binary = cli_helpers::detect()
            .path
            .and_then(|p| p.to_str().map(str::to_owned))
            .unwrap_or_else(|| "container".to_string());
        Self {
            cli: ContainerCli::new(binary),
            system_ready: AtomicBool::new(false),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            cli: ContainerCli::new(binary),
            system_ready: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl ContainerRuntime for AppleContainersRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::AppleContainers
    }

    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
        // On non-mac hosts the runtime is definitively unavailable.
        #[cfg(not(target_os = "macos"))]
        {
            return Ok(RuntimeAvailability {
                available: false,
                version: None,
                reason: Some("Apple Containers requires macOS 26 or later".into()),
            });
        }
        #[cfg(target_os = "macos")]
        {
            match run_capturing(&self.cli, ["--version"]).await {
                Ok((stdout, _)) => Ok(RuntimeAvailability {
                    available: true,
                    version: Some(stdout.trim().to_string()).filter(|s| !s.is_empty()),
                    reason: None,
                }),
                Err(e) => Ok(RuntimeAvailability {
                    available: false,
                    version: None,
                    reason: Some(format!("`{}` not runnable: {e}", self.cli.binary())),
                }),
            }
        }
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), ContainerRuntimeError> {
        let args = cli::pull_args(image);
        run_capturing(&self.cli, args.iter().map(String::as_str)).await?;
        Ok(())
    }

    async fn ensure_system_running(
        &self,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<(), ContainerRuntimeError> {
        if self.system_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        // `container system status` is the cheapest probe: it prints a
        // table whose first data row is `status running` (or `stopped`).
        // Failure to parse is treated as "not running" so we attempt a
        // start; that is harmless when the daemon really is up because
        // `container system start` is idempotent and exits 0.
        let status_text = match run_capturing(&self.cli, ["system", "status"]).await {
            Ok((stdout, _)) => stdout,
            Err(_) => String::new(),
        };
        let already_running = cli::system_status_is_running(&status_text);
        if !already_running {
            if let Some(sink) = &log_sink {
                let _ = sink
                    .send(LogChunk {
                        stream: LogStreamKind::System,
                        line: "container system not running; starting…".into(),
                    })
                    .await;
            }
        }
        info!(
            binary = self.cli.binary(),
            already_running, "ensuring container system + default kernel"
        );
        // Delegate to the shared helper so we get:
        //   * `--enable-kernel-install` (avoids the "failed to read user
        //     input" first-run failure when launched from a GUI app
        //     with no controlling TTY), and
        //   * a self-heal pass that runs `container system kernel set
        //     --recommended` if the default kernel is still missing —
        //     covers users left in the half-initialised state by
        //     earlier app builds.
        // The helper is idempotent and cheap when everything is
        // already in place, so it's fine to run on every cold cache
        // hit (we still gate on `system_ready` above).
        // Bridge cli_helpers' line-by-line String sink into our
        // LogChunk sink so the kernel-download progress shows up in
        // the embedding app's UI rather than disappearing into the
        // void until exit.
        let bridge_handle = log_sink.as_ref().map(|chunk_tx| {
            let (str_tx, mut str_rx) = mpsc::channel::<String>(64);
            let chunk_tx = chunk_tx.clone();
            let handle = tokio::spawn(async move {
                while let Some(line) = str_rx.recv().await {
                    if chunk_tx
                        .send(LogChunk {
                            stream: LogStreamKind::System,
                            line,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            (str_tx, handle)
        });
        let str_sink = bridge_handle.as_ref().map(|(tx, _)| tx.clone());
        let result = cli_helpers::ensure_service_running_with_log(
            std::path::Path::new(self.cli.binary()),
            str_sink,
        )
        .await;
        // Drop the original sender so the bridge task exits once
        // cli_helpers' internal pumps drain.
        if let Some((tx, handle)) = bridge_handle {
            drop(tx);
            let _ = handle.await;
        }
        result.map_err(ContainerRuntimeError::Backend)?;
        if !already_running {
            if let Some(sink) = &log_sink {
                let _ = sink
                    .send(LogChunk {
                        stream: LogStreamKind::System,
                        line: "container system started".into(),
                    })
                    .await;
            }
        }
        self.system_ready.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn image_exists(&self, image: &ImageRef) -> Result<bool, ContainerRuntimeError> {
        let (stdout, _) =
            match run_capturing(&self.cli, ["image", "list", "--format", "json"]).await {
                Ok(p) => p,
                Err(e) => return Err(e),
            };
        let needle = cli::image_ref_to_string(image);
        Ok(cli::image_list_contains(&stdout, &needle))
    }

    async fn image_label(
        &self,
        image: &ImageRef,
        key: &str,
    ) -> Result<Option<String>, ContainerRuntimeError> {
        let needle = cli::image_ref_to_string(image);
        let (stdout, _) = match run_capturing(&self.cli, ["image", "inspect", &needle]).await {
            Ok(p) => p,
            // Apple returns a non-zero exit when the image is absent;
            // surface that as `Ok(None)` rather than a hard error so
            // callers can treat "absent" and "label missing" the same.
            Err(_) => return Ok(None),
        };
        Ok(cli::image_label_from_inspect(&stdout, key))
    }

    /// Build an image via `container build`. Streams stdout/stderr lines
    /// to `log_sink` as they arrive so the dashboard can show progress in
    /// real time, then returns the resulting [`ImageRef`] (== `spec.tag`).
    async fn build(
        &self,
        spec: &BuildSpec,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<ImageRef, ContainerRuntimeError> {
        let args = cli::build_args(spec);
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        info!(
            binary = self.cli.binary(),
            argv = ?args,
            "running container build"
        );

        let mut child = cmd
            .spawn()
            .map_err(|e| ContainerRuntimeError::Backend(format!("spawn `container build`: {e}")))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stdout from `container build`".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stderr from `container build`".into())
        })?;

        // Pump output to the optional sink. Even when no sink is wired up
        // we still need to drain the pipes so the child does not block on
        // a full buffer; we collect stderr into a String so we can include
        // it in the error on failure.
        let stderr_buf: std::sync::Arc<parking_lot::Mutex<String>> =
            std::sync::Arc::new(parking_lot::Mutex::new(String::new()));

        let stdout_sink = log_sink.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(tx) = &stdout_sink {
                    if tx
                        .send(LogChunk {
                            stream: LogStreamKind::Stdout,
                            line,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        let stderr_sink = log_sink.clone();
        let stderr_buf2 = stderr_buf.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                {
                    let mut buf = stderr_buf2.lock();
                    buf.push_str(&line);
                    buf.push('\n');
                }
                if let Some(tx) = &stderr_sink {
                    if tx
                        .send(LogChunk {
                            stream: LogStreamKind::Stderr,
                            line,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        let status = child
            .wait()
            .await
            .map_err(|e| ContainerRuntimeError::Backend(format!("wait `container build`: {e}")))?;

        if !status.success() {
            let stderr = stderr_buf.lock().clone();
            warn!(
                binary = self.cli.binary(),
                argv = ?args,
                %status,
                stderr = %stderr.trim(),
                "container build exited non-zero",
            );
            return Err(ContainerRuntimeError::Backend(format!(
                "`{} {}` exited with {status}: {}",
                self.cli.binary(),
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(spec.tag.clone())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, ContainerRuntimeError> {
        let args = cli::create_args(spec);
        let (stdout, _) = run_capturing(&self.cli, args.iter().map(String::as_str)).await?;
        let id = stdout
            .trim()
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() {
            return Err(ContainerRuntimeError::Backend(
                "`container create` returned no container id".into(),
            ));
        }
        Ok(id)
    }

    async fn start(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        ensure_container_id(container_id)?;
        run_capturing(&self.cli, ["start", container_id]).await?;
        // Apple's `container start` returns as soon as the start request is
        // accepted, but the runtime may still be transitioning the
        // container into "running" state when we immediately try to
        // `exec` the postCreateCommand. Poll inspect briefly so we
        // don't race the lifecycle hook into "cannot exec: container is
        // not running".
        for attempt in 0u32..20 {
            match self.inspect(container_id).await {
                Ok(status) if matches!(status.state, ContainerState::Running) => return Ok(()),
                Ok(_) | Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    debug!(
                        container = container_id,
                        attempt, "waiting for running state"
                    );
                }
            }
        }
        warn!(
            container = container_id,
            "container did not reach Running state within timeout; continuing anyway"
        );
        Ok(())
    }

    async fn stop(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        ensure_container_id(container_id)?;
        run_capturing(&self.cli, ["stop", container_id]).await?;
        Ok(())
    }

    async fn remove(&self, container_id: &str, force: bool) -> Result<(), ContainerRuntimeError> {
        ensure_container_id(container_id)?;
        let args = cli::remove_args(container_id, force);
        run_capturing(&self.cli, args.iter().map(String::as_str)).await?;
        Ok(())
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerStatus, ContainerRuntimeError> {
        ensure_container_id(container_id)?;
        let (stdout, _) = run_capturing(&self.cli, ["inspect", container_id]).await?;
        cli::parse_inspect(&stdout, container_id)
    }

    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
        let (stdout, _) = run_capturing(&self.cli, ["list", "--all", "--format", "json"]).await?;
        cli::parse_list(&stdout)
    }

    async fn exec(
        &self,
        container_id: &str,
        options: &ExecOptions,
    ) -> Result<ExecResult, ContainerRuntimeError> {
        let args = cli::exec_args(container_id, options);
        info!(
            binary = self.cli.binary(),
            argv = ?args,
            "running container exec"
        );

        // Fast path: no streaming, no cancellation -> single-shot
        // `output()` is the simplest correct thing.
        if options.log_sink.is_none() && options.cancel.is_none() {
            let mut cmd = self.cli.command();
            for a in &args {
                cmd.arg(a);
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let output = cmd.output().await.map_err(|e| {
                ContainerRuntimeError::Backend(format!("spawn `container exec`: {e}"))
            })?;
            let exit_code = output.status.code().unwrap_or(-1);
            if exit_code != 0 {
                warn!(
                    binary = self.cli.binary(),
                    argv = ?args,
                    exit_code,
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "container exec exited non-zero",
                );
            }
            return Ok(ExecResult {
                exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }

        // Streaming / cancellable path. Spawn the child with piped
        // stdio, fan stdout+stderr through line-readers that:
        //   1. forward each line to `options.log_sink` (if any), and
        //   2. accumulate the bytes for the eventual `ExecResult`.
        // Race the child against `options.cancel`; on cancellation we
        // drop the child (kill_on_drop terminates it) and return
        // `Cancelled` so the orchestrator can surface a clean status.
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| ContainerRuntimeError::Backend(format!("spawn `container exec`: {e}")))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stdout from `container exec`".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stderr from `container exec`".into())
        })?;

        let stdout_buf = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let stdout_h = spawn_stream_pump(
            stdout,
            LogStreamKind::Stdout,
            stdout_buf.clone(),
            options.log_sink.clone(),
        );
        let stderr_h = spawn_stream_pump(
            stderr,
            LogStreamKind::Stderr,
            stderr_buf.clone(),
            options.log_sink.clone(),
        );

        let exit_status = if let Some(notify) = options.cancel.clone() {
            tokio::select! {
                biased;
                _ = notify.notified() => {
                    // Dropping `child` (with kill_on_drop) terminates
                    // the local `container exec` process. The pumps
                    // exit when their pipes close.
                    drop(child);
                    let _ = stdout_h.await;
                    let _ = stderr_h.await;
                    return Err(ContainerRuntimeError::Cancelled);
                }
                res = child.wait() => res,
            }
        } else {
            child.wait().await
        }
        .map_err(|e| ContainerRuntimeError::Backend(format!("await `container exec`: {e}")))?;

        let _ = stdout_h.await;
        let _ = stderr_h.await;

        let exit_code = exit_status.code().unwrap_or(-1);
        let stdout_bytes = std::mem::take(&mut *stdout_buf.lock());
        let stderr_bytes = std::mem::take(&mut *stderr_buf.lock());
        if exit_code != 0 {
            warn!(
                binary = self.cli.binary(),
                argv = ?args,
                exit_code,
                stderr = %String::from_utf8_lossy(&stderr_bytes).trim(),
                "container exec exited non-zero",
            );
        }
        Ok(ExecResult {
            exit_code,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }

    async fn logs(
        &self,
        container_id: &str,
        options: &LogOptions,
    ) -> Result<LogStream, ContainerRuntimeError> {
        let args = cli::logs_args(container_id, options);
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| ContainerRuntimeError::Backend(format!("spawn `container logs`: {e}")))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stdout from `container logs`".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stderr from `container logs`".into())
        })?;

        // Bound the channel so a slow consumer cannot grow memory without
        // limit. When the consumer drops the receiver, `send` will fail and
        // the spawned task will exit, which drops `child` and (with
        // `kill_on_drop`) terminates the `container logs` process.
        let (tx, rx) = mpsc::channel::<LogChunk>(256);

        spawn_line_pump(stdout, LogStreamKind::Stdout, tx.clone());
        spawn_line_pump(stderr, LogStreamKind::Stderr, tx.clone());
        tokio::spawn(async move {
            let _ = child.wait().await;
            drop(tx);
        });

        Ok(rx)
    }
}

fn spawn_line_pump<R>(reader: R, kind: LogStreamKind, tx: mpsc::Sender<LogChunk>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(LogChunk { stream: kind, line }).await.is_err() {
                break;
            }
        }
    });
}

/// Pump that fans each line into both an accumulating byte buffer
/// (for the eventual `ExecResult`) and an optional caller-supplied
/// streaming sink. Used by `exec` when the caller wants to see output
/// live (e.g. lifecycle hooks). Returns a join handle so callers can
/// await full drain of the pipe before reading the buffer.
fn spawn_stream_pump<R>(
    reader: R,
    kind: LogStreamKind,
    buf: Arc<parking_lot::Mutex<Vec<u8>>>,
    sink: Option<mpsc::Sender<LogChunk>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            {
                let mut b = buf.lock();
                b.extend_from_slice(line.as_bytes());
                b.push(b'\n');
            }
            if let Some(tx) = sink.as_ref() {
                // A closed sink shouldn't terminate the pump — we
                // still want to keep reading so the child's pipe
                // doesn't fill and block. Just stop forwarding.
                let _ = tx.send(LogChunk { stream: kind, line }).await;
            }
        }
    })
}

async fn run_capturing<I, S>(
    cli: &ContainerCli,
    args: I,
) -> Result<(String, String), ContainerRuntimeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = cli.command();
    let mut argv: Vec<String> = Vec::new();
    for a in args {
        let s = a.as_ref().to_string_lossy().into_owned();
        cmd.arg(a);
        argv.push(s);
    }
    debug!(binary = %cli.binary(), argv = ?argv, "running container CLI");
    let output = cmd.output().await.map_err(|e| {
        ContainerRuntimeError::Backend(format!("failed to run `{}`: {e}", cli.binary()))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        warn!(
            binary = %cli.binary(),
            argv = ?argv,
            status = %output.status,
            stderr = %stderr,
            "container CLI exited non-zero"
        );
        return Err(ContainerRuntimeError::Backend(format!(
            "`{} {}` exited with {}: {}",
            cli.binary(),
            argv.join(" "),
            output.status,
            stderr,
        )));
    }
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// A `serde`-compatible shape for the subset of `container inspect` /
/// `container list` output the orchestrator cares about.
///
/// Apple's CLI nests most identity fields under a `configuration` object
/// (id, image, etc.) while runtime state lives at the top level
/// (`status`, `startedDate`, `networks`). Older builds and our own
/// fixtures sometimes put `id`/`image` at the top level too, so both
/// shapes are accepted; the nested values win when present.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectShape {
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub image: Option<InspectImage>,
    #[serde(default)]
    pub configuration: Option<InspectConfiguration>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectConfiguration {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub image: Option<InspectImage>,
    /// Bind/volume mounts attached to the container. We only consume the
    /// `source` (host-side path) for repo linkage; the rest is ignored.
    #[serde(default)]
    pub mounts: Vec<InspectMount>,
    /// Per-container labels stamped at create time. Key/value strings.
    /// The orchestrator reads `org.devcontainers.config_hash` from here
    /// to detect drift without app-side state.
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectMount {
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InspectImage {
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl InspectShape {
    pub(crate) fn into_status(self) -> ContainerStatus {
        let state = match self
            .status
            .as_deref()
            .or(self.state.as_deref())
            .map(|s| s.to_ascii_lowercase())
        {
            Some(ref s) if s == "running" => ContainerState::Running,
            Some(ref s) if s == "stopped" => ContainerState::Stopped,
            Some(ref s) if s == "exited" => ContainerState::Exited,
            Some(ref s) if s == "created" => ContainerState::Created,
            _ => ContainerState::Unknown,
        };
        let (cfg_id, cfg_image, cfg_mounts, cfg_labels) = match self.configuration {
            Some(c) => (c.id, c.image, c.mounts, c.labels),
            None => (None, None, Vec::new(), std::collections::HashMap::new()),
        };
        let container_id = cfg_id.or(self.id).or(self.name).unwrap_or_default();
        let image_ref = cfg_image
            .or(self.image)
            .and_then(|i| i.reference.or(i.name));
        let host_mounts = cfg_mounts
            .into_iter()
            .filter_map(|m| m.source.filter(|s| !s.is_empty()))
            .collect();
        ContainerStatus {
            container_id,
            state,
            image_ref,
            host_mounts,
            labels: cfg_labels,
        }
    }
}

/// Reject calls into the runtime with an empty container id. Apple's
/// `container` CLI surfaces an empty argv as `notFound: container with
/// ID  not found`, which is confusing and indistinguishable from a
/// genuinely missing container; bail out earlier with a clear message
/// so the dashboard can ignore the call (and the bug that produced the
/// empty id is easier to spot).
fn ensure_container_id(id: &str) -> Result<(), ContainerRuntimeError> {
    if id.trim().is_empty() {
        return Err(ContainerRuntimeError::Backend(
            "missing container id (empty string passed to runtime)".into(),
        ));
    }
    Ok(())
}

/// Format a [`crate::container::MountSpec`] as the `--mount` flag value
/// accepted by `container create`.
pub(crate) fn mount_flag(m: &super::traits::MountSpec) -> String {
    let kind = match m.kind {
        MountKind::Bind => "bind",
        MountKind::Volume => "volume",
    };
    let mut s = format!(
        "type={},source={},target={}",
        kind,
        m.source.display(),
        m.target.display()
    );
    if m.read_only {
        s.push_str(",readonly");
    }
    s
}
