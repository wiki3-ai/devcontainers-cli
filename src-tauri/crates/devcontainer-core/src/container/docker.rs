//! Docker backend.
//!
//! Drives the `docker` CLI rather than talking to the daemon over its API
//! socket. That matches the existing Apple Containers backend design,
//! keeps every operation inspectable as an argv (which is what the unit
//! tests below pin), and avoids adding a Docker client dependency for the
//! handful of operations the orchestrator needs.
//!
//! Docker is the reference runtime for Dev Container compatibility: the
//! `--mount` / `--publish` / `--env` forms the translation layer emits are
//! Docker's own native syntax, and `runArgs` are passed straight through —
//! so unlike the Apple backend this one has no deny-list.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::exec_probe;
use super::traits::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ContainerSpec, ContainerState,
    ContainerStatus, ExecOptions, ExecResult, ImageRef, LogChunk, LogOptions, LogStream,
    LogStreamKind, MountKind, MountSpec, RuntimeAvailability, RuntimeId,
};

/// Locations probed before falling back to `PATH`.
///
/// Docker Desktop on macOS ships its CLI inside the app bundle; Homebrew
/// and manual installs land in the usual `bin` directories. The bundle
/// path matters because a Finder-launched app inherits launchd's minimal
/// `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains none of these.
const DOCKER_STANDARD_PATHS: &[&str] = &[
    "/Applications/Docker.app/Contents/Resources/bin/docker",
    "/usr/local/bin/docker",
    "/opt/homebrew/bin/docker",
];

#[derive(Debug, Clone)]
pub struct DockerCli {
    binary: String,
}

impl DockerCli {
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    pub fn binary(&self) -> &str {
        &self.binary
    }

    pub fn command(&self) -> Command {
        Command::new(&self.binary)
    }
}

impl Default for DockerCli {
    fn default() -> Self {
        Self::new("docker")
    }
}

// ---------------------------------------------------------------------------
// argv construction
//
// Pure functions over the spec types, so the wire format is testable with
// no Docker daemon anywhere in sight.
// ---------------------------------------------------------------------------

/// `[registry/]repo[:tag][@digest]`.
pub(crate) fn image_ref_to_string(image: &ImageRef) -> String {
    let mut s = String::new();
    if let Some(reg) = &image.registry {
        s.push_str(reg);
        s.push('/');
    }
    s.push_str(&image.repository);
    if let Some(tag) = &image.tag {
        s.push(':');
        s.push_str(tag);
    }
    if let Some(d) = &image.digest {
        s.push('@');
        s.push_str(d);
    }
    s
}

pub(crate) fn pull_args(image: &ImageRef) -> Vec<String> {
    vec!["pull".to_string(), image_ref_to_string(image)]
}

/// `docker build --tag <ref> --file <dockerfile> [--build-arg k=v]...
/// [--target T] [--label k=v]... <context>`
///
/// Build args and labels are sorted so the argv is deterministic
/// regardless of `HashMap` iteration order.
pub(crate) fn build_args(spec: &BuildSpec) -> Vec<String> {
    let mut a = vec![
        "build".to_string(),
        "--tag".to_string(),
        image_ref_to_string(&spec.tag),
        "--file".to_string(),
        spec.dockerfile.display().to_string(),
    ];

    let mut build_args: Vec<(&String, &String)> = spec.build_args.iter().collect();
    build_args.sort_by(|x, y| x.0.cmp(y.0));
    for (k, v) in build_args {
        a.push("--build-arg".to_string());
        a.push(format!("{k}={v}"));
    }

    if let Some(target) = &spec.target {
        a.push("--target".to_string());
        a.push(target.clone());
    }

    let mut labels: Vec<(&String, &String)> = spec.labels.iter().collect();
    labels.sort_by(|x, y| x.0.cmp(y.0));
    for (k, v) in labels {
        a.push("--label".to_string());
        a.push(format!("{k}={v}"));
    }

    a.push(spec.context_dir.display().to_string());
    a
}

/// `type=<bind|volume>,source=...,target=...[,readonly]` — the value for
/// `--mount`. Same syntax as the Apple backend, and Podman accepts it too.
pub(crate) fn mount_flag(m: &MountSpec) -> String {
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

/// `docker create --name N ... <image> [cmd...]`
///
/// `runArgs` are inserted immediately before the image so they behave like
/// `docker run` flags. Everything the translation layer emits here is
/// native Docker syntax.
pub(crate) fn create_args(spec: &ContainerSpec) -> Vec<String> {
    let mut a = vec![
        "create".to_string(),
        "--name".to_string(),
        spec.name.clone(),
    ];

    for (k, v) in &spec.env {
        a.push("--env".to_string());
        a.push(format!("{k}={v}"));
    }
    sort_kv_block(&mut a, "--env");

    for m in &spec.mounts {
        a.push("--mount".to_string());
        a.push(mount_flag(m));
    }

    for p in &spec.ports {
        let proto = match p.protocol {
            crate::container::PortProtocol::Tcp => "tcp",
            crate::container::PortProtocol::Udp => "udp",
        };
        a.push("--publish".to_string());
        a.push(format!("{}:{}/{proto}", p.host_port, p.container_port));
    }

    if let Some(workdir) = &spec.workdir {
        a.push("--workdir".to_string());
        a.push(workdir.display().to_string());
    }
    if let Some(user) = &spec.user {
        a.push("--user".to_string());
        a.push(user.clone());
    }
    if spec.privileged {
        a.push("--privileged".to_string());
    }

    let mut labels: Vec<(&String, &String)> = spec.labels.iter().collect();
    labels.sort_by(|x, y| x.0.cmp(y.0));
    for (k, v) in labels {
        a.push("--label".to_string());
        a.push(format!("{k}={v}"));
    }

    for raw in &spec.run_args {
        a.push(raw.clone());
    }

    a.push(image_ref_to_string(&spec.image));
    // `None` means "leave the image's ENTRYPOINT/CMD alone" — required by
    // projects that set `overrideCommand: false` so their own long-running
    // command actually starts.
    if let Some(cmd) = &spec.command {
        for arg in cmd {
            a.push(arg.clone());
        }
    }
    a
}

pub(crate) fn remove_args(container_id: &str, force: bool) -> Vec<String> {
    let mut a = vec!["rm".to_string()];
    if force {
        a.push("--force".to_string());
    }
    a.push(container_id.to_string());
    a
}

pub(crate) fn exec_args(container_id: &str, options: &ExecOptions) -> Vec<String> {
    let mut a = vec!["exec".to_string()];
    if options.tty {
        a.push("--interactive".to_string());
        a.push("--tty".to_string());
    }
    if let Some(workdir) = &options.workdir {
        a.push("--workdir".to_string());
        a.push(workdir.display().to_string());
    }
    if let Some(user) = &options.user {
        a.push("--user".to_string());
        a.push(user.clone());
    }
    let mut envs: Vec<(&String, &String)> = options.env.iter().collect();
    envs.sort_by(|x, y| x.0.cmp(y.0));
    for (k, v) in envs {
        a.push("--env".to_string());
        a.push(format!("{k}={v}"));
    }
    a.push(container_id.to_string());
    for arg in &options.command {
        a.push(arg.clone());
    }
    a
}

pub(crate) fn logs_args(container_id: &str, options: &LogOptions) -> Vec<String> {
    let mut a = vec!["logs".to_string()];
    if options.follow {
        a.push("--follow".to_string());
    }
    if let Some(t) = options.tail {
        a.push("--tail".to_string());
        a.push(t.to_string());
    }
    a.push(container_id.to_string());
    a
}

/// Sort consecutive `flag value` pairs in place so emitted argv is
/// deterministic for tests. Pairs not matching `flag` are left alone.
fn sort_kv_block(args: &mut Vec<String>, flag: &str) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let start = i;
            let mut end = i;
            while end + 1 < args.len() && args[end] == flag {
                end += 2;
            }
            let mut pairs: Vec<(String, String)> = args[start..end]
                .chunks(2)
                .map(|c| (c[0].clone(), c[1].clone()))
                .collect();
            pairs.sort_by(|a, b| a.1.cmp(&b.1));
            let mut flat: Vec<String> = Vec::with_capacity(end - start);
            for (k, v) in pairs {
                flat.push(k);
                flat.push(v);
            }
            args.splice(start..end, flat);
            i = end;
        } else {
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// `docker inspect` parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct InspectEntry {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "State")]
    state: Option<InspectState>,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<InspectMount>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(rename = "Status")]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Image")]
    image: Option<String>,
    /// Docker emits `"Labels": null` for containers created without
    /// labels, so this has to tolerate null rather than defaulting to `{}`.
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InspectMount {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Source", default)]
    source: String,
}

/// Map Docker's `State.Status` onto the runtime-agnostic enum.
///
/// Docker has no "stopped" status — a stopped container is `exited` — and
/// `paused` / `restarting` / `removing` are transient states the
/// orchestrator does not model, so they map to `Unknown` rather than being
/// forced into a wrong bucket.
pub(crate) fn map_state(status: Option<&str>) -> ContainerState {
    match status.unwrap_or("").to_ascii_lowercase().as_str() {
        "created" => ContainerState::Created,
        "running" => ContainerState::Running,
        "exited" | "dead" => ContainerState::Exited,
        _ => ContainerState::Unknown,
    }
}

pub(crate) fn parse_inspect(json: &str) -> Result<Vec<ContainerStatus>, String> {
    let entries: Vec<InspectEntry> =
        serde_json::from_str(json).map_err(|e| format!("invalid `docker inspect` JSON: {e}"))?;
    Ok(entries
        .into_iter()
        .map(|e| ContainerStatus {
            container_id: e.id,
            state: map_state(e.state.as_ref().and_then(|s| s.status.as_deref())),
            image_ref: e.config.as_ref().and_then(|c| c.image.clone()),
            host_mounts: e
                .mounts
                .into_iter()
                .filter(|m| m.kind == "bind")
                .map(|m| m.source)
                .collect(),
            labels: e.config.and_then(|c| c.labels).unwrap_or_default(),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DockerRuntime {
    cli: DockerCli,
}

impl Default for DockerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerRuntime {
    /// Resolve the `docker` binary via the standard install locations and
    /// `PATH`, falling back to the bare name so error messages still make
    /// sense when nothing was found.
    pub fn new() -> Self {
        let binary = exec_probe::probe_binary_in_env("docker", DOCKER_STANDARD_PATHS)
            .path_str()
            .map(str::to_owned)
            .unwrap_or_else(|| "docker".to_string());
        Self {
            cli: DockerCli::new(binary),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            cli: DockerCli::new(binary),
        }
    }

    /// Whether a `docker` executable exists on this host. Cheap, and
    /// deliberately does not require the daemon to be up — see
    /// [`ContainerRuntime::ensure_system_running`] for that distinction.
    pub fn detect() -> exec_probe::ExecutableProbe {
        exec_probe::probe_binary_in_env("docker", DOCKER_STANDARD_PATHS)
    }

    async fn run<I, S>(&self, args: I) -> Result<String, ContainerRuntimeError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_capturing(&self.cli, args).await
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::Docker
    }

    async fn probe(&self) -> Result<RuntimeAvailability, ContainerRuntimeError> {
        match run_capturing(&self.cli, ["--version"]).await {
            Ok(stdout) => Ok(RuntimeAvailability {
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

    /// Docker Desktop is started by the user, not by us: auto-launching a
    /// GUI app and then blocking on it is a surprise better left as an
    /// explicit choice. So we check, and if the daemon is down we fail with
    /// a message that says what to do.
    async fn ensure_system_running(
        &self,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<(), ContainerRuntimeError> {
        // `docker info` talks to the daemon; `docker --version` does not.
        if self
            .run(["info", "--format", "{{.ServerVersion}}"])
            .await
            .is_ok()
        {
            return Ok(());
        }
        let message = format!(
            "the Docker daemon is not reachable via `{}`. Start Docker Desktop \
             (or `dockerd`) and try again.",
            self.cli.binary()
        );
        if let Some(tx) = log_sink {
            let _ = tx
                .send(LogChunk {
                    stream: LogStreamKind::System,
                    line: message.clone(),
                })
                .await;
        }
        Err(ContainerRuntimeError::Unavailable(message))
    }

    async fn image_exists(&self, image: &ImageRef) -> Result<bool, ContainerRuntimeError> {
        match self
            .run(["image", "inspect", &image_ref_to_string(image)])
            .await
        {
            Ok(_) => Ok(true),
            // A missing image is an expected answer, not a failure.
            Err(_) => Ok(false),
        }
    }

    async fn image_label(
        &self,
        image: &ImageRef,
        key: &str,
    ) -> Result<Option<String>, ContainerRuntimeError> {
        let json = match self
            .run([
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                &image_ref_to_string(image),
            ])
            .await
        {
            Ok(j) => j,
            Err(_) => return Ok(None),
        };
        let labels: Option<HashMap<String, String>> = serde_json::from_str(json.trim()).ok();
        Ok(labels.and_then(|l| l.get(key).cloned()))
    }

    async fn pull(&self, image: &ImageRef) -> Result<(), ContainerRuntimeError> {
        let args = pull_args(image);
        self.run(args).await?;
        Ok(())
    }

    async fn build(
        &self,
        spec: &BuildSpec,
        log_sink: Option<mpsc::Sender<LogChunk>>,
    ) -> Result<ImageRef, ContainerRuntimeError> {
        let args = build_args(spec);
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            ContainerRuntimeError::Backend(format!("spawn `{} build`: {e}", self.cli.binary()))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stdout from `docker build`".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ContainerRuntimeError::Backend("no stderr from `docker build`".into())
        })?;

        let h_out = spawn_line_pump(stdout, LogStreamKind::Stdout, log_sink.clone());
        let h_err = spawn_line_pump(stderr, LogStreamKind::Stderr, log_sink);
        let status = child
            .wait()
            .await
            .map_err(|e| ContainerRuntimeError::Backend(format!("await `docker build`: {e}")))?;
        let _ = h_out.await;
        let _ = h_err.await;

        if !status.success() {
            return Err(ContainerRuntimeError::Backend(format!(
                "`{} build` exited with {}",
                self.cli.binary(),
                status.code().unwrap_or(-1)
            )));
        }
        debug!(tag = %image_ref_to_string(&spec.tag), "docker build succeeded");
        Ok(spec.tag.clone())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String, ContainerRuntimeError> {
        let args = create_args(spec);
        let stdout = self.run(args).await?;
        // `docker create` prints the new container ID on stdout.
        Ok(stdout.trim().to_string())
    }

    async fn start(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        self.run(["start", container_id]).await?;
        Ok(())
    }

    async fn stop(&self, container_id: &str) -> Result<(), ContainerRuntimeError> {
        self.run(["stop", container_id]).await?;
        Ok(())
    }

    async fn remove(&self, container_id: &str, force: bool) -> Result<(), ContainerRuntimeError> {
        let args = remove_args(container_id, force);
        self.run(args).await?;
        Ok(())
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerStatus, ContainerRuntimeError> {
        let json = self.run(["inspect", container_id]).await?;
        let mut statuses = parse_inspect(&json).map_err(ContainerRuntimeError::Backend)?;
        statuses.pop().ok_or_else(|| {
            ContainerRuntimeError::Backend(format!("no such container: {container_id}"))
        })
    }

    async fn list(&self) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
        let ids_out = self.run(["ps", "--all", "--quiet"]).await?;
        let ids: Vec<String> = ids_out
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // One `inspect` for every id: cheaper than N round-trips, and it
        // yields the same JSON shape the single-container path parses.
        let mut args = vec!["inspect".to_string()];
        args.extend(ids);
        let json = self.run(args).await?;
        parse_inspect(&json).map_err(ContainerRuntimeError::Backend)
    }

    async fn exec(
        &self,
        container_id: &str,
        options: &ExecOptions,
    ) -> Result<ExecResult, ContainerRuntimeError> {
        let args = exec_args(container_id, options);
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            ContainerRuntimeError::Backend(format!("spawn `{} exec`: {e}", self.cli.binary()))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ContainerRuntimeError::Backend("no stdout from `docker exec`".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ContainerRuntimeError::Backend("no stderr from `docker exec`".into()))?;

        let stdout_buf = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let h_out = spawn_stream_pump(
            stdout,
            LogStreamKind::Stdout,
            Arc::clone(&stdout_buf),
            options.log_sink.clone(),
        );
        let h_err = spawn_stream_pump(
            stderr,
            LogStreamKind::Stderr,
            Arc::clone(&stderr_buf),
            options.log_sink.clone(),
        );

        let exit_status = if let Some(notify) = options.cancel.clone() {
            tokio::select! {
                biased;
                _ = notify.notified() => {
                    // Dropping `child` (with kill_on_drop) terminates the
                    // local `docker exec` client. The process inside the
                    // container is the orchestrator's concern, not ours.
                    drop(child);
                    let _ = h_out.await;
                    let _ = h_err.await;
                    return Err(ContainerRuntimeError::Cancelled);
                }
                res = child.wait() => res,
            }
        } else {
            child.wait().await
        }
        .map_err(|e| ContainerRuntimeError::Backend(format!("await `docker exec`: {e}")))?;

        let _ = h_out.await;
        let _ = h_err.await;

        let exit_code = exit_status.code().unwrap_or(-1);
        let stdout_bytes = std::mem::take(&mut *stdout_buf.lock());
        let stderr_bytes = std::mem::take(&mut *stderr_buf.lock());
        if exit_code != 0 {
            warn!(
                binary = self.cli.binary(),
                argv = ?args,
                exit_code,
                stderr = %String::from_utf8_lossy(&stderr_bytes).trim(),
                "docker exec exited non-zero",
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
        let args = logs_args(container_id, options);
        let mut cmd = self.cli.command();
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            ContainerRuntimeError::Backend(format!("spawn `{} logs`: {e}", self.cli.binary()))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ContainerRuntimeError::Backend("no stdout from `docker logs`".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ContainerRuntimeError::Backend("no stderr from `docker logs`".into()))?;

        // Bounded so a slow consumer cannot grow memory without limit.
        // Dropping the receiver makes `send` fail, the pumps exit, and
        // `child` (with kill_on_drop) terminates `docker logs`.
        let (tx, rx) = mpsc::channel::<LogChunk>(256);
        let h_out = spawn_line_pump(stdout, LogStreamKind::Stdout, Some(tx.clone()));
        let h_err = spawn_line_pump(stderr, LogStreamKind::Stderr, Some(tx.clone()));
        tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = h_out.await;
            let _ = h_err.await;
            drop(tx);
        });

        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// Run a Docker subcommand to completion and return its stdout.
///
/// A non-zero exit becomes an error carrying the CLI's stderr, because
/// that is where Docker puts the explanation ("No such container", the
/// failed `RUN` line, the registry auth failure, …).
async fn run_capturing<I, S>(cli: &DockerCli, args: I) -> Result<String, ContainerRuntimeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = cli.command();
    let mut argv: Vec<String> = Vec::new();
    for a in args {
        let s = a.as_ref().to_string_lossy().into_owned();
        cmd.arg(a.as_ref());
        argv.push(s);
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| ContainerRuntimeError::Backend(format!("spawn `{}`: {e}", cli.binary())))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(ContainerRuntimeError::Backend(format!(
            "`{} {}` failed with {}{}",
            cli.binary(),
            argv.join(" "),
            output.status.code().unwrap_or(-1),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Stream a pipe line-by-line into `sink`, or just drain it when there is
/// no sink so the child's pipe never fills and blocks the process.
fn spawn_line_pump<R>(
    reader: R,
    kind: LogStreamKind,
    sink: Option<mpsc::Sender<LogChunk>>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // A closed sink must not stop us reading: keep draining so the
            // child is not blocked on a full pipe.
            if let Some(tx) = sink.as_ref() {
                let _ = tx.send(LogChunk { stream: kind, line }).await;
            }
        }
    })
}

/// Like [`spawn_line_pump`] but also accumulates the bytes, so the same
/// read serves both live streaming and the eventual [`ExecResult`].
fn spawn_stream_pump<R>(
    reader: R,
    kind: LogStreamKind,
    buf: Arc<parking_lot::Mutex<Vec<u8>>>,
    sink: Option<mpsc::Sender<LogChunk>>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
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
                let _ = tx.send(LogChunk { stream: kind, line }).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{PortForward, PortProtocol};
    use std::path::PathBuf;

    fn ubuntu() -> ImageRef {
        ImageRef {
            registry: None,
            repository: "ubuntu".into(),
            tag: Some("24.04".into()),
            digest: None,
        }
    }

    fn minimal_spec() -> ContainerSpec {
        ContainerSpec {
            name: "dev".into(),
            image: ubuntu(),
            command: None,
            workdir: None,
            env: HashMap::new(),
            mounts: vec![],
            ports: vec![],
            user: None,
            privileged: false,
            run_args: vec![],
            labels: HashMap::new(),
        }
    }

    #[test]
    fn image_ref_roundtrips_including_registry_and_digest() {
        assert_eq!(image_ref_to_string(&ubuntu()), "ubuntu:24.04");
        let r = ImageRef {
            registry: Some("ghcr.io".into()),
            repository: "wiki3-ai/app".into(),
            tag: Some("1.0".into()),
            digest: Some("sha256:deadbeef".into()),
        };
        assert_eq!(
            image_ref_to_string(&r),
            "ghcr.io/wiki3-ai/app:1.0@sha256:deadbeef"
        );
    }

    #[test]
    fn pull_args_are_just_the_ref() {
        assert_eq!(pull_args(&ubuntu()), vec!["pull", "ubuntu:24.04"]);
    }

    #[test]
    fn build_args_place_context_last_and_sort_kv_blocks() {
        let mut build_arg_pairs = HashMap::new();
        build_arg_pairs.insert("B".to_string(), "2".to_string());
        build_arg_pairs.insert("A".to_string(), "1".to_string());
        let mut labels = HashMap::new();
        labels.insert("org.example".to_string(), "yes".to_string());
        let spec = BuildSpec {
            tag: ubuntu(),
            context_dir: PathBuf::from("/repo/.devcontainer"),
            dockerfile: PathBuf::from("/repo/.devcontainer/Dockerfile"),
            build_args: build_arg_pairs,
            target: Some("dev".into()),
            labels,
        };

        let a = build_args(&spec);
        assert_eq!(a[0], "build");
        assert_eq!(
            a[a.iter().position(|x| x == "--tag").unwrap() + 1],
            "ubuntu:24.04"
        );
        assert_eq!(
            a[a.iter().position(|x| x == "--file").unwrap() + 1],
            "/repo/.devcontainer/Dockerfile"
        );
        assert_eq!(
            a[a.iter().position(|x| x == "--target").unwrap() + 1],
            "dev"
        );
        // Build args are sorted by key, so A comes before B.
        let first_arg = a.iter().position(|x| x == "--build-arg").unwrap();
        assert_eq!(a[first_arg + 1], "A=1");
        assert_eq!(a[first_arg + 3], "B=2");
        assert_eq!(a.last().unwrap(), "/repo/.devcontainer");
    }

    #[test]
    fn external_filesystem_build_has_no_dns_flags() {
        // Unlike the Apple backend, Docker resolves DNS itself; there is
        // no `--dns` injection to regress.
        let spec = BuildSpec {
            tag: ubuntu(),
            context_dir: PathBuf::from("/ctx"),
            dockerfile: PathBuf::from("/ctx/Dockerfile"),
            build_args: HashMap::new(),
            target: None,
            labels: HashMap::new(),
        };
        assert!(!build_args(&spec).iter().any(|x| x == "--dns"));
    }

    #[test]
    fn mount_flag_emits_docker_mount_syntax() {
        let bind = MountSpec {
            kind: MountKind::Bind,
            source: PathBuf::from("/host/src"),
            target: PathBuf::from("/work"),
            read_only: false,
        };
        assert_eq!(mount_flag(&bind), "type=bind,source=/host/src,target=/work");

        let volume = MountSpec {
            kind: MountKind::Volume,
            source: PathBuf::from("hermes-opt-data"),
            target: PathBuf::from("/opt/data"),
            read_only: false,
        };
        assert_eq!(
            mount_flag(&volume),
            "type=volume,source=hermes-opt-data,target=/opt/data"
        );

        let ro = MountSpec {
            read_only: true,
            ..bind
        };
        assert!(mount_flag(&ro).ends_with(",readonly"));
    }

    #[test]
    fn create_args_sorts_env_mounts_the_workspace_and_puts_image_last() {
        let mut env = HashMap::new();
        env.insert("Z".to_string(), "z".to_string());
        env.insert("A".to_string(), "a".to_string());
        let mut labels = HashMap::new();
        labels.insert("org.devcontainers.config_hash".into(), "abc".into());
        let spec = ContainerSpec {
            name: "wiki-dev".into(),
            image: ubuntu(),
            command: Some(vec!["bash".into(), "-lc".into()]),
            workdir: Some(PathBuf::from("/workspaces/repo")),
            env,
            mounts: vec![MountSpec {
                kind: MountKind::Bind,
                source: PathBuf::from("/host/repo"),
                target: PathBuf::from("/workspaces/repo"),
                read_only: false,
            }],
            ports: vec![PortForward {
                host_port: 9119,
                container_port: 9119,
                protocol: PortProtocol::Tcp,
            }],
            user: Some("vscode".into()),
            privileged: false,
            run_args: vec![],
            labels,
        };

        let a = create_args(&spec);
        assert_eq!(a[0], "create");
        assert_eq!(a[1], "--name");
        assert_eq!(a[2], "wiki-dev");

        // Env sorted by value so the argv is deterministic.
        let first_env = a.iter().position(|x| x == "--env").unwrap();
        assert_eq!(a[first_env + 1], "A=a");
        assert_eq!(a[first_env + 3], "Z=z");

        // Workspace bind mount is present with Docker's `--mount` syntax.
        let mount_idx = a.iter().position(|x| x == "--mount").unwrap();
        assert_eq!(
            a[mount_idx + 1],
            "type=bind,source=/host/repo,target=/workspaces/repo"
        );

        let pub_idx = a.iter().position(|x| x == "--publish").unwrap();
        assert_eq!(a[pub_idx + 1], "9119:9119/tcp");
        assert_eq!(
            a[a.iter().position(|x| x == "--workdir").unwrap() + 1],
            "/workspaces/repo"
        );
        assert_eq!(
            a[a.iter().position(|x| x == "--user").unwrap() + 1],
            "vscode"
        );

        // Image last, then the command after it.
        assert_eq!(a[a.len() - 3], "ubuntu:24.04");
        assert_eq!(&a[a.len() - 2..], ["bash", "-lc"]);
    }

    #[test]
    fn create_args_omits_the_command_when_override_command_is_false() {
        // `overrideCommand: false` => ContainerSpec.command is None, and
        // no replacement may be invented: the image's own CMD must run.
        let spec = minimal_spec();
        let a = create_args(&spec);
        assert_eq!(a.last().unwrap(), "ubuntu:24.04");
        assert!(
            !a.iter().any(|x| x.contains("sleep")),
            "no keepalive may be injected: {a:?}"
        );
    }

    #[test]
    fn create_args_honours_privileged() {
        // Docker supports --privileged natively (Apple Containers does not),
        // which is part of why Docker is the reference backend.
        let spec = ContainerSpec {
            privileged: true,
            ..minimal_spec()
        };
        assert!(create_args(&spec).iter().any(|x| x == "--privileged"));
    }

    #[test]
    fn create_args_passes_run_args_through_before_the_image() {
        let spec = ContainerSpec {
            run_args: vec![
                "--add-host=host.docker.internal:host-gateway".into(),
                "--cpus=4".into(),
            ],
            ..minimal_spec()
        };
        let a = create_args(&spec);
        let image_idx = a.iter().position(|x| x == "ubuntu:24.04").unwrap();
        let add_host_idx = a
            .iter()
            .position(|x| x == "--add-host=host.docker.internal:host-gateway")
            .unwrap();
        assert!(
            add_host_idx < image_idx,
            "runArgs must precede the image or docker parses them as a command: {a:?}"
        );
    }

    #[test]
    fn exec_args_include_tty_workdir_user_env_and_command() {
        let mut env = HashMap::new();
        env.insert("K".to_string(), "V".to_string());
        let options = ExecOptions {
            command: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            workdir: Some(PathBuf::from("/work")),
            env,
            user: Some("vscode".into()),
            tty: true,
            log_sink: None,
            cancel: None,
        };
        let a = exec_args("cid", &options);
        assert_eq!(a[0], "exec");
        assert!(a.iter().any(|x| x == "--tty"));
        assert_eq!(
            a[a.iter().position(|x| x == "--workdir").unwrap() + 1],
            "/work"
        );
        assert_eq!(
            a[a.iter().position(|x| x == "--user").unwrap() + 1],
            "vscode"
        );
        assert_eq!(a[a.iter().position(|x| x == "--env").unwrap() + 1], "K=V");
        // Container id, then the process argv.
        let cid = a.iter().position(|x| x == "cid").unwrap();
        assert_eq!(&a[cid + 1..], ["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn logs_args_carry_follow_and_tail() {
        let options = LogOptions {
            follow: true,
            tail: Some(50),
        };
        assert_eq!(
            logs_args("cid", &options),
            vec!["logs", "--follow", "--tail", "50", "cid"]
        );
        let options = LogOptions {
            follow: false,
            tail: None,
        };
        assert_eq!(logs_args("cid", &options), vec!["logs", "cid"]);
    }

    #[test]
    fn remove_args_honour_force() {
        assert_eq!(remove_args("cid", false), vec!["rm", "cid"]);
        assert_eq!(remove_args("cid", true), vec!["rm", "--force", "cid"]);
    }

    #[test]
    fn map_state_covers_docker_status_strings() {
        assert_eq!(map_state(Some("created")), ContainerState::Created);
        assert_eq!(map_state(Some("running")), ContainerState::Running);
        assert_eq!(map_state(Some("exited")), ContainerState::Exited);
        assert_eq!(map_state(Some("dead")), ContainerState::Exited);
        // Transient/unknown states must not be forced into a wrong bucket.
        assert_eq!(map_state(Some("paused")), ContainerState::Unknown);
        assert_eq!(map_state(Some("restarting")), ContainerState::Unknown);
        assert_eq!(map_state(None), ContainerState::Unknown);
    }

    #[test]
    fn parse_inspect_reads_state_image_labels_and_bind_mounts() {
        let json = r#"[
            {
                "Id": "abc123",
                "State": { "Status": "running" },
                "Config": {
                    "Image": "hermes-devcontainer:latest",
                    "Labels": { "org.devcontainers.config_hash": "deadbeef" }
                },
                "Mounts": [
                    { "Type": "bind", "Source": "/host/repo", "Destination": "/workspaces/repo" },
                    { "Type": "volume", "Source": "hermes-opt-data", "Destination": "/opt/data" }
                ]
            }
        ]"#;
        let statuses = parse_inspect(json).expect("parse");
        assert_eq!(statuses.len(), 1);
        let s = &statuses[0];
        assert_eq!(s.container_id, "abc123");
        assert_eq!(s.state, ContainerState::Running);
        assert_eq!(s.image_ref.as_deref(), Some("hermes-devcontainer:latest"));
        assert_eq!(
            s.labels
                .get("org.devcontainers.config_hash")
                .map(String::as_str),
            Some("deadbeef")
        );
        // Only bind mounts count as host mounts; a named volume is not a
        // host path and must not be reported as one.
        assert_eq!(s.host_mounts, vec!["/host/repo".to_string()]);
    }

    #[test]
    fn parse_inspect_tolerates_null_labels() {
        // Docker emits `"Labels": null` when a container has none, which
        // would fail a plain `HashMap` deserialisation.
        let json = r#"[{ "Id": "x", "State": { "Status": "exited" }, "Config": { "Image": "i", "Labels": null } }]"#;
        let statuses = parse_inspect(json).expect("parse");
        assert_eq!(statuses[0].state, ContainerState::Exited);
        assert!(statuses[0].labels.is_empty());
    }

    #[test]
    fn parse_inspect_rejects_malformed_json() {
        assert!(parse_inspect("not json").is_err());
    }
}
