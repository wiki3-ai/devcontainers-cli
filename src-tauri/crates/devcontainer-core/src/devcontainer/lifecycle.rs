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
//! All event emission is funnelled through
//! [`crate::events::EventSink`] so the orchestrator body can be
//! unit-tested without a real Tauri app, and so sibling apps can
//! provide their own emission impls.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::container::{
    BuildSpec, ContainerRuntime, ContainerRuntimeError, ExecOptions, ImageRef, LogChunk,
    LogStreamKind, RuntimeRegistry,
};
use crate::devcontainer::translate::{
    parse_image_ref, to_container_spec, DevContainerBuild, LifecycleCommand, ParsedDevContainer,
};
use crate::events::EventSink;

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
    #[error("hook {label} cancelled by user")]
    HookCancelled { label: &'static str },
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

/// Names of the buildkit-predeclared "proxy build args". These reach
/// RUN steps in the Dockerfile without requiring matching `ARG`
/// lines, and are stripped from the recorded image config so they
/// don't bake into the layer metadata.
const PROXY_VAR_NAMES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "FTP_PROXY",
    "ALL_PROXY",
];

/// IPv4 of the Apple Containers default bridge gateway. Loopback
/// proxy URLs are rewritten to use this literal IP rather than a
/// hostname because `container build`'s sandbox does not pick up the
/// `container system dns create` registration: that mechanism only
/// configures the host's `mDNSResponder`, not the build VM's
/// `/etc/resolv.conf`. Using the gateway IP directly sidesteps DNS
/// entirely. Apple's `container` runtime hard-codes `192.168.64.0/24`
/// for the default bridge with `.1` as the gateway.
const HOST_BRIDGE_GATEWAY_IP: &str = "192.168.64.1";

/// `NO_PROXY` value used when the internal proxy is auto-injected.
/// Bypasses the proxy for localhost, IPv6 loopback, and the bridge
/// gateway itself so containers can still talk directly to host
/// services and to each other.
const INTERNAL_PROXY_NO_PROXY: &str = "localhost,127.0.0.1,::1,192.168.64.1";

/// Compute the effective proxy env for an `up`, given a host-env
/// source and an optional internal-proxy URL. Pure function; the
/// orchestrator wraps this with `std::env::var` and the lazy proxy
/// start. Tests use it directly to avoid mutating global env.
///
/// Precedence: any host-supplied proxy var wins (with loopback
/// rewriting). If the host has no proxy var set at all, the internal
/// URL is injected as `HTTP_PROXY` + `HTTPS_PROXY` + a sensible
/// `NO_PROXY`. If the host is unset *and* the internal URL is
/// `None`, the result is empty.
///
/// For every populated variable we emit *both* the uppercase
/// (`HTTP_PROXY`) and lowercase (`http_proxy`) form. BuildKit's
/// predeclared proxy build-args are uppercase-only, but tools at
/// runtime — `curl`, `wget`, `pip`, `git`, many shell scripts —
/// honour only the lowercase form, so the runtime container env
/// path needs both. Callers that only want one case (e.g. the
/// build-args path via [`merge_proxy_build_args`]) iterate the
/// uppercase canonical list and ignore the rest.
pub(crate) fn compute_effective_proxy_env<F>(
    mut host_env: F,
    internal_url: Option<String>,
) -> HashMap<String, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = HashMap::new();
    let mut host_set = false;
    let mut insert_both = |name: &str, value: String| {
        out.insert(name.to_ascii_uppercase(), value.clone());
        out.insert(name.to_ascii_lowercase(), value);
    };
    for name in PROXY_VAR_NAMES {
        let v = host_env(name)
            .or_else(|| host_env(&name.to_ascii_lowercase()))
            .filter(|v| !v.is_empty());
        if let Some(v) = v {
            host_set = true;
            insert_both(name, rewrite_localhost_to_host_gateway(&v));
        }
    }
    if !host_set {
        if let Some(url) = internal_url {
            // Loopback rewrite for symmetry with the host-supplied
            // path. In production the manager binds on the bridge
            // gateway already, so this is a no-op; in tests that
            // bind on `127.0.0.1` (and conceivably for the Phase 4
            // launchd shared service) it keeps the URL reachable
            // from inside the container.
            let url = rewrite_localhost_to_host_gateway(&url);
            insert_both("HTTP_PROXY", url.clone());
            insert_both("HTTPS_PROXY", url);
            insert_both("NO_PROXY", INTERNAL_PROXY_NO_PROXY.into());
        }
    }
    if !out.is_empty() {
        tracing::info!(
            source = if host_set { "host" } else { "internal" },
            http_proxy = out.get("HTTP_PROXY").map(String::as_str).unwrap_or(""),
            "proxy: effective env for up"
        );
    } else {
        tracing::debug!("proxy: no effective env (host unset, internal disabled)");
    }
    out
}

/// Merge proxy environment variables (HTTP_PROXY etc.) into a
/// build-args map. Each name is looked up via `lookup` in both upper-
/// and lower-case forms; the first non-empty value wins. Existing
/// keys in `args` are never overwritten — caller-supplied build args
/// take precedence over the host environment.
///
/// Host literals of `localhost`, `127.0.0.1`, and `::1` in the URL
/// are rewritten to the Apple Containers bridge gateway IP
/// (`192.168.64.1`) so a host-side proxy reachable on the loopback
/// interface can be reached from inside the build container without
/// depending on DNS — `container build` does not pick up the
/// `host.docker.internal` registration.
pub(crate) fn merge_proxy_build_args<F>(
    mut args: HashMap<String, String>,
    mut lookup: F,
) -> HashMap<String, String>
where
    F: FnMut(&str) -> Option<String>,
{
    for name in PROXY_VAR_NAMES {
        if args.contains_key(*name) {
            continue;
        }
        let value = lookup(name)
            .or_else(|| lookup(&name.to_ascii_lowercase()))
            .filter(|v| !v.is_empty());
        if let Some(value) = value {
            args.insert(
                (*name).to_string(),
                rewrite_localhost_to_host_gateway(&value),
            );
        }
    }
    args
}

/// Rewrite `localhost` / `127.0.0.1` / `::1` host components in a
/// proxy URL to the bridge gateway IP. See [`merge_proxy_build_args`].
fn rewrite_localhost_to_host_gateway(url: &str) -> String {
    let mut out = String::with_capacity(url.len() + 16);
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `://` or `@` host-literal boundaries.
        let host_start = if bytes[i..].starts_with(b"://") {
            out.push_str("://");
            i + 3
        } else if bytes[i] == b'@' {
            out.push('@');
            i + 1
        } else {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        };
        let mut host_end = host_start;
        while host_end < bytes.len() && !matches!(bytes[host_end], b':' | b'/' | b'?' | b'#') {
            host_end += 1;
        }
        let host = &url[host_start..host_end];
        let rewritten = match host.to_ascii_lowercase().as_str() {
            "localhost" | "127.0.0.1" | "[::1]" | "::1" => HOST_BRIDGE_GATEWAY_IP,
            _ => host,
        };
        out.push_str(rewritten);
        i = host_end;
    }
    out
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
    /// `true` when the running/stopped container's stamped
    /// `config_hash` label disagrees with the current contents of
    /// `devcontainer.json` (and Dockerfile, if `build:`). `false` when
    /// the labels match, `None` when we couldn't decide (no live
    /// container, no recorded hash, etc.). Drives the dashboard's
    /// non-modal "Configuration changed — Rebuild?" banner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_drift: Option<bool>,
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
    /// Cancellation handle published while a long-running lifecycle
    /// hook (e.g. `postCreateCommand`) is executing. Calling
    /// [`LifecycleOrchestrator::cancel_hook`] fires it; the
    /// streaming-aware `runtime.exec` races the child against this
    /// notification and kills it on cancel. `None` when no hook is
    /// in flight.
    cancel: Option<Arc<tokio::sync::Notify>>,
}

#[derive(Default)]
pub struct LifecycleOrchestrator {
    /// Per-workspace persistent fields (parsed config, current container id,
    /// last status) read/written under a short-lived `parking_lot::RwLock`.
    slots: RwLock<HashMap<String, WorkspaceSlot>>,
    /// Per-workspace tokio mutex serialising mutating commands. Held across
    /// awaits so we use `tokio::sync::Mutex` rather than `parking_lot`.
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    /// Lazy-started internal forward proxy. When the host has no
    /// `HTTP_PROXY` of its own, this manager's bound URL is injected
    /// into containers' env at `up` time so package managers route
    /// through it. Defaults to disabled (tests + headless callers);
    /// production callers construct via [`Self::with_proxy`].
    proxy: Arc<crate::devcontainer::proxy_manager::ProxyManager>,
    /// Optional in-memory override of the host environment. When
    /// `Some`, [`Self::effective_proxy_env`] looks up proxy vars
    /// here instead of via `std::env::var`. Used by integration tests
    /// to avoid racing on process-global state. Production never
    /// touches this.
    host_env_override: RwLock<Option<HashMap<String, String>>>,
}

impl std::fmt::Debug for LifecycleOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleOrchestrator")
            .field("workspaces", &self.slots.read().len())
            .field("proxy", &self.proxy)
            .finish()
    }
}

impl LifecycleOrchestrator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an orchestrator wired to the given proxy manager.
    /// Use [`crate::devcontainer::proxy_manager::ProxyManager::with_apple_containers_default`]
    /// for the standard `192.168.64.1:31280` bind.
    pub fn with_proxy(proxy: crate::devcontainer::proxy_manager::ProxyManager) -> Self {
        Self {
            proxy: Arc::new(proxy),
            ..Self::default()
        }
    }

    /// Access the lazy-started proxy manager. Useful for the UI
    /// status pill and "flush cache" command.
    pub fn proxy(&self) -> &Arc<crate::devcontainer::proxy_manager::ProxyManager> {
        &self.proxy
    }

    /// Resolve the proxy env vars (`HTTP_PROXY` etc.) that should be
    /// merged into builds and containers for this `up`.
    ///
    /// Precedence: any host env var the user already set wins. If the
    /// host has *no* proxy set, we lazy-start the internal proxy and
    /// fall back to its URL. If the internal proxy is disabled or its
    /// bind failed, the resulting map is empty.
    ///
    /// Loopback hostnames in host-supplied URLs are rewritten to the
    /// Apple Containers bridge gateway IP — see
    /// [`rewrite_localhost_to_host_gateway`].
    async fn effective_proxy_env(&self) -> HashMap<String, String> {
        // Determine whether the host already has a proxy set before
        // touching the internal proxy: starting it requires a tcp
        // bind, and there's no point paying for that when the user
        // (or their corporate config) already has one.
        let override_map = self.host_env_override.read().clone();
        let host_lookup = |n: &str| -> Option<String> {
            match &override_map {
                Some(m) => m.get(n).cloned(),
                None => std::env::var(n).ok(),
            }
        };
        let host_has_proxy = PROXY_VAR_NAMES.iter().any(|n| {
            host_lookup(n).filter(|v| !v.is_empty()).is_some()
                || host_lookup(&n.to_ascii_lowercase())
                    .filter(|v| !v.is_empty())
                    .is_some()
        });
        let internal_url = if host_has_proxy {
            None
        } else {
            self.proxy.ensure_started().await
        };
        compute_effective_proxy_env(host_lookup, internal_url)
    }

    /// Test-only: install an in-memory map that replaces the process
    /// environment for proxy-var lookups. The map is consulted with
    /// the same name conventions as `std::env::var` (case-sensitive;
    /// callers that want loose matching should populate both forms).
    /// Pass an empty map to simulate "host has no proxy".
    pub fn set_host_env_override(&self, env: HashMap<String, String>) {
        *self.host_env_override.write() = Some(env);
    }

    /// Store the parsed config for `workspace_id`. Replaces any prior value.
    pub fn set_parsed_config(&self, workspace_id: &str, parsed: ParsedDevContainer) {
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.parsed = Some(parsed);
    }

    /// Record the host-side workspace path for `workspace_id`. Needs
    /// to land on the slot before the user clicks Stop / Rebuild on a
    /// freshly-restarted app, so the orchestrator can derive the
    /// container name and adopt the live container that was created
    /// in a previous app run.
    pub fn record_host_workspace(&self, workspace_id: &str, host_workspace: &Path) {
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.host_workspace = Some(host_workspace.to_path_buf());
    }

    /// Cancel any in-flight lifecycle hook for `workspace_id`. Fires
    /// the workspace's published cancel notify (set by the
    /// streaming-aware `run_hook` / `spawn_detached_hook` while a hook
    /// is executing) so the apple-containers exec child is killed and
    /// the orchestrator returns a clean cancelled status. No-op when
    /// nothing is registered.
    ///
    /// Importantly, this does **not** take the per-workspace lock —
    /// `up_with_sink` already holds it. The whole point of cancel is
    /// to break a stuck Up out of its hook.
    pub fn cancel_hook(&self, workspace_id: &str) -> bool {
        let cancel = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.cancel.clone());
        if let Some(notify) = cancel {
            notify.notify_waiters();
            true
        } else {
            false
        }
    }

    fn install_cancel(&self, workspace_id: &str) -> Arc<tokio::sync::Notify> {
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut map = self.slots.write();
        let slot = map.entry(workspace_id.to_string()).or_default();
        slot.cancel = Some(notify.clone());
        notify
    }

    fn clear_cancel(&self, workspace_id: &str) {
        if let Some(slot) = self.slots.write().get_mut(workspace_id) {
            slot.cancel = None;
        }
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
                config_drift: None,
            };
        }
        LifecycleStatus {
            workspace_id: workspace_id.to_string(),
            state: "absent",
            container_id: None,
            image_ref: None,
            error: None,
            config_drift: None,
        }
    }

    /// Status with drift detection. Inspects the live container (if any),
    /// reads its stamped `org.devcontainers.config_hash` label, and
    /// compares it against a freshly-recomputed hash of
    /// `devcontainer.json` (+ Dockerfile, when applicable). On any
    /// failure to determine the answer (no live container, label
    /// missing, parsed config absent) drift is reported as `None`
    /// rather than `false` so the UI can distinguish "definitely up to
    /// date" from "we don't know".
    pub async fn status_with_drift(
        &self,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> LifecycleStatus {
        let mut snap = self.snapshot(workspace_id);
        let parsed = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.parsed.clone());

        let runtime = registry.selected();
        // Make sure host_workspace is on the slot before we try to
        // derive a container name from it. This also means subsequent
        // Stop/Rebuild calls — which read host_workspace off the slot
        // — work after a plain `container_status` poll, even when the
        // app was just restarted and `up` hasn't run this session.
        self.record_host_workspace(workspace_id, host_workspace);
        // Look up the container by recorded id, falling back to the
        // derived name. We don't ensure_system_running here — drift
        // checks happen on a poll and shouldn't surface as an error or
        // boot the daemon as a side-effect; if inspect fails we just
        // leave drift unset.
        //
        // Adoption runs even without a parsed config: at app startup
        // the frontend hasn't submitted devcontainer.json yet, but a
        // container created in a previous session is still findable by
        // its deterministic derived name, so we surface its real state
        // rather than "absent".
        let recorded = self
            .slots
            .read()
            .get(workspace_id)
            .and_then(|s| s.container_id.clone());
        let target = recorded
            .clone()
            .unwrap_or_else(|| derive_container_name(workspace_id, host_workspace));
        let live = match runtime.inspect(&target).await {
            Ok(s) => s,
            Err(_) => return snap,
        };
        // Adopt: persist the live id so subsequent Stop / Rebuild
        // calls don't have to re-derive it. Also surface it on the
        // returned snapshot.
        let live_id = if live.container_id.is_empty() {
            target.clone()
        } else {
            live.container_id.clone()
        };
        if recorded.as_deref() != Some(live_id.as_str()) {
            let mut map = self.slots.write();
            let slot = map.entry(workspace_id.to_string()).or_default();
            slot.container_id = Some(live_id.clone());
            if slot.last_image_ref.is_none() {
                slot.last_image_ref = live.image_ref.clone();
            }
            if slot.last_state.is_none() {
                slot.last_state = Some(match live.state {
                    crate::container::ContainerState::Running => "running",
                    crate::container::ContainerState::Stopped => "stopped",
                    crate::container::ContainerState::Created => "created",
                    crate::container::ContainerState::Exited => "exited",
                    crate::container::ContainerState::Unknown => "unknown",
                });
            }
        }
        if snap.container_id.is_none() {
            snap.container_id = Some(live_id);
        }
        if snap.image_ref.is_none() {
            snap.image_ref = live.image_ref.clone();
        }
        // Reflect the live runtime state on the snapshot too — without
        // this the dashboard keeps reporting "absent" for a container
        // that the user can clearly see is running in `container ls`.
        snap.state = match live.state {
            crate::container::ContainerState::Running => "running",
            crate::container::ContainerState::Stopped => "stopped",
            crate::container::ContainerState::Created => "created",
            crate::container::ContainerState::Exited => "exited",
            crate::container::ContainerState::Unknown => "unknown",
        };
        let stamped = live.labels.get(LABEL_CONFIG_HASH).cloned();
        // Drift is undecidable without a parsed config; that's the
        // expected state right after app startup before the frontend
        // has had a chance to submit one. Adoption above has already
        // populated the live state on the snapshot.
        let Some(parsed) = parsed else {
            return snap;
        };
        let dockerfile_path: Option<std::path::PathBuf> = parsed.build.as_ref().map(|b| {
            let cfg_dir: std::path::PathBuf = parsed
                .config_file_path
                .as_ref()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| host_workspace.join(".devcontainer"));
            cfg_dir.join(b.dockerfile.as_deref().unwrap_or("Dockerfile"))
        });
        let current = compute_config_hash(&parsed, dockerfile_path.as_deref());
        snap.config_drift = match (stamped, current) {
            (Some(s), Some(c)) => Some(s != c),
            _ => None,
        };
        snap
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

    /// Drive `pull → create → start` and run the post-create/start/attach
    /// lifecycle hooks against the selected runtime. Sink-parameterised so
    /// tests can capture the emitted events without a real Tauri app.
    ///
    /// The sink is passed as `Arc<dyn EventSink>` (rather than
    /// `&dyn EventSink`) so detached lifecycle hooks — e.g. a
    /// long-running `postStartCommand` that spawns `jupyter lab` —
    /// can clone the handle and continue streaming output to the
    /// host's UI after this method returns.
    pub async fn up_with_sink(
        &self,
        sink: Arc<dyn EventSink>,
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
            .up_inner(sink.clone(), registry, workspace_id, host_workspace)
            .await;
        if let Err(err) = &result {
            self.report_failure(sink.as_ref(), workspace_id, "up", err);
        }
        result
    }

    /// Decide whether to pull a pre-built image or build one from a
    /// Dockerfile, returning the resulting [`ImageRef`]. Build output
    /// is streamed to `sink` line-by-line.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_image(
        &self,
        sink: &dyn EventSink,
        runtime: &dyn ContainerRuntime,
        workspace_id: &str,
        host_workspace: &Path,
        parsed: &ParsedDevContainer,
        config_hash: Option<&str>,
        proxy_env: &HashMap<String, String>,
    ) -> Result<ImageRef, LifecycleError> {
        if let Some(build) = parsed.build.as_ref() {
            let mut build_spec =
                self.resolve_build_spec(workspace_id, host_workspace, parsed, build, proxy_env);
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
        proxy_env: &HashMap<String, String>,
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
            build_args: merge_proxy_build_args(build.args.clone(), |n| proxy_env.get(n).cloned()),
            target: build.target.clone(),
            labels: HashMap::new(),
        }
    }

    async fn up_inner(
        &self,
        sink: Arc<dyn EventSink>,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        // Most call sites in this method want a `&dyn EventSink`. Hold
        // the Arc for the entire body so it stays alive, and reborrow
        // a reference for the synchronous helpers. The detached
        // postStart/postAttach hooks below clone the Arc itself.
        let sink_arc = sink;
        let sink: &dyn EventSink = sink_arc.as_ref();
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

        // Resolve the proxy env once for this `up`. Preferring host-
        // configured proxies and falling back to the internal proxy
        // is the same shape for both `container build` and the
        // running container, so we compute the map once and use it
        // in both places.
        let proxy_env = self.effective_proxy_env().await;

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
                &proxy_env,
            )
            .await?;
        let mut spec = to_container_spec(&parsed, image_ref.clone(), workspace_id, host_workspace);
        // Forward proxy env vars into the container so that lifecycle
        // hooks (e.g. `postCreateCommand: pip install ...`) can reach
        // the network. We inject *both* `HTTP_PROXY` and `http_proxy`
        // forms because tools like `curl`, `wget`, `pip`, and `git`
        // only honour the lowercase variants. Explicit entries in
        // `containerEnv` / `remoteEnv` win over both host and
        // internal-proxy values.
        for (k, v) in &proxy_env {
            spec.env.entry(k.clone()).or_insert_with(|| v.clone());
        }
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
        let mut freshly_created = true;
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
                freshly_created = false;
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
        // inside the container via `runtime.exec`, split into two groups:
        //
        //   * "create-time" hooks (`onCreateCommand`, `updateContentCommand`,
        //     `postCreateCommand`) run **once per container instance**. We
        //     stamp `/var/devcontainer/postcreate_done` after they succeed
        //     and skip them on subsequent starts (e.g., after the user
        //     stops + starts the same container, or after we adopt an
        //     existing container by name on app boot).
        //   * "start-time" hooks (`postStartCommand`, `postAttachCommand`)
        //     run on **every** start.
        //
        // This matches the devcontainer spec's intent: postCreateCommand
        // is for one-time setup like `npm install`, while postStartCommand
        // is for things that should run each time the container boots.
        let create_hooks_already_ran = if freshly_created {
            false
        } else {
            postcreate_sentinel_exists(runtime.as_ref(), &container_id)
                .await
                .unwrap_or(false)
        };
        if !create_hooks_already_ran {
            for (label, hook) in [
                ("onCreateCommand", parsed.on_create_command.as_ref()),
                (
                    "updateContentCommand",
                    parsed.update_content_command.as_ref(),
                ),
                ("postCreateCommand", parsed.post_create_command.as_ref()),
            ] {
                if let Some(cmd) = hook {
                    run_hook(
                        self,
                        sink_arc.clone(),
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
            // Write the sentinel even if no hooks were defined; that way
            // we don't have to re-evaluate whether *this* config has any
            // create-time hooks the next time around.
            if let Err(err) = write_postcreate_sentinel(runtime.as_ref(), &container_id).await {
                warn!(
                    workspace = workspace_id,
                    container = %container_id,
                    error = %err,
                    "failed to stamp postcreate sentinel; create-time hooks may re-run"
                );
            }
        } else {
            sink.log(
                workspace_id,
                LogStreamKind::System,
                "skipping onCreate/updateContent/postCreate (already ran for this container)",
            );
        }
        for (label, hook) in [
            ("postStartCommand", parsed.post_start_command.as_ref()),
            ("postAttachCommand", parsed.post_attach_command.as_ref()),
        ] {
            if let Some(cmd) = hook {
                // postStart / postAttach commonly run long-lived
                // entrypoints (e.g. `jupyter lab`) that never exit.
                // Awaiting them here would hold the per-workspace
                // lock indefinitely and silently block every
                // subsequent Stop / Restart / Rebuild / Remove. Spawn
                // them detached: output continues streaming via the
                // Arc'd sink, and the orchestrator returns control.
                spawn_detached_hook(
                    sink_arc.clone(),
                    runtime.clone(),
                    workspace_id,
                    &container_id,
                    &spec,
                    label,
                    cmd,
                );
            }
        }

        info!(workspace = workspace_id, container = %container_id, "lifecycle.up running");
        Ok(LifecycleStatus {
            workspace_id: workspace_id.to_string(),
            state: "running",
            container_id: Some(container_id),
            image_ref: Some(spec.image.repository),
            error: None,
            config_drift: Some(false),
        })
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
        // Prefer the recorded id, but fall back to the derived
        // container name when the slot is empty (typical after an app
        // restart: the user's container is still running but we
        // haven't done an `up` this session). Without this fallback
        // the Stop button is a silent no-op.
        let cid = self
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
        if let Some(cid) = cid {
            ensure_runtime_ready(sink, runtime.as_ref(), workspace_id).await?;
            info!(workspace = workspace_id, container = %cid, "lifecycle.stop");
            match runtime.stop(&cid).await {
                Ok(()) => {}
                // Treat "not found" as success — the container we were
                // about to stop doesn't exist; the desired end state
                // (not running) is already true. This shows up after
                // someone runs `container delete` outside the app.
                Err(ContainerRuntimeError::Backend(msg)) if is_not_found(&msg) => {
                    info!(
                        workspace = workspace_id,
                        container = %cid,
                        "stop: container not found; treating as success"
                    );
                    sink.log(
                        workspace_id,
                        LogStreamKind::System,
                        &format!("container `{cid}` not found (already gone)"),
                    );
                    self.record_state(workspace_id, "absent", None, None);
                    sink.status(workspace_id, "absent", None, None, None);
                    return Ok(self.snapshot(workspace_id));
                }
                Err(err) => {
                    let lerr = stage::<()>("stop", Err(err)).unwrap_err();
                    self.report_failure(sink, workspace_id, "stop", &lerr);
                    return Err(lerr);
                }
            }
            self.record_state(workspace_id, "stopped", Some(&cid), None);
            sink.status(workspace_id, "stopped", Some(&cid), None, None);
        } else {
            debug!(workspace = workspace_id, "stop: no container recorded");
            self.record_state(workspace_id, "absent", None, None);
        }
        Ok(self.snapshot(workspace_id))
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

    pub async fn rebuild_with_sink(
        &self,
        sink: Arc<dyn EventSink>,
        registry: &RuntimeRegistry,
        workspace_id: &str,
        host_workspace: &Path,
    ) -> Result<LifecycleStatus, LifecycleError> {
        // `up` and `remove` already grab the per-workspace lock, so call
        // them sequentially without holding it ourselves.
        info!(workspace = workspace_id, "lifecycle.rebuild");
        self.remove_with_sink(sink.as_ref(), registry, workspace_id)
            .await?;
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

/// Path of the in-container sentinel that marks "create-time hooks
/// have run for this container instance". A regular file under `/var`
/// is durable for the container's lifetime but does *not* persist
/// across `remove` + `create`, which is exactly the semantics the
/// devcontainer spec asks for: postCreate runs once per container.
const POSTCREATE_SENTINEL: &str = "/var/devcontainer/postcreate_done";

/// Probe for `POSTCREATE_SENTINEL`. Returns `Ok(true)` only when the
/// sentinel definitely exists; any error or non-zero exit becomes
/// `Ok(false)` so we err on the side of running the hooks again rather
/// than silently skipping them.
async fn postcreate_sentinel_exists(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
) -> Result<bool, ContainerRuntimeError> {
    let opts = ExecOptions {
        command: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("test -f {POSTCREATE_SENTINEL}"),
        ],
        workdir: None,
        env: std::collections::HashMap::new(),
        user: None,
        tty: false,
        ..Default::default()
    };
    match runtime.exec(container_id, &opts).await {
        Ok(r) => Ok(r.exit_code == 0),
        Err(_) => Ok(false),
    }
}

/// Write `POSTCREATE_SENTINEL` after create-time hooks succeed. We
/// best-effort here: if the write fails (e.g., a read-only `/var`,
/// missing `sh`) we log a warning at the call site and accept that
/// create-time hooks may run again on the next start.
async fn write_postcreate_sentinel(
    runtime: &dyn ContainerRuntime,
    container_id: &str,
) -> Result<(), ContainerRuntimeError> {
    let opts = ExecOptions {
        command: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("mkdir -p $(dirname {POSTCREATE_SENTINEL}) && : > {POSTCREATE_SENTINEL}"),
        ],
        workdir: None,
        env: std::collections::HashMap::new(),
        user: None,
        tty: false,
        ..Default::default()
    };
    let result = runtime.exec(container_id, &opts).await?;
    if result.exit_code != 0 {
        return Err(ContainerRuntimeError::Backend(format!(
            "failed to write postcreate sentinel (exit {})",
            result.exit_code
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_hook(
    orchestrator: &LifecycleOrchestrator,
    sink: Arc<dyn EventSink>,
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
    // Stream stdout/stderr lines into `sink` as they appear so the
    // user sees progress on long-running hooks (e.g. `pip install`)
    // instead of staring at "running postCreateCommand: …" until exit.
    // Also publish a cancel handle on the workspace slot for the
    // duration of the hook so the UI can break a stuck hook.
    let cancel = orchestrator.install_cancel(workspace_id);
    let (tx, mut rx) = mpsc::channel::<LogChunk>(256);
    let relay_workspace = workspace_id.to_string();
    let relay_sink = sink.clone();
    let relay = tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if !chunk.line.is_empty() {
                relay_sink.log(&relay_workspace, chunk.stream, &chunk.line);
            }
        }
    });
    let opts = ExecOptions {
        command: argv,
        workdir: spec.workdir.clone(),
        env: spec.env.clone(),
        user: spec.user.clone(),
        tty: false,
        log_sink: Some(tx),
        cancel: Some(cancel),
    };
    let exec_res = runtime.exec(container_id, &opts).await;
    // Drop the sender by dropping `opts` (which owns it). `relay`
    // then completes once the channel drains.
    drop(opts);
    let _ = relay.await;
    orchestrator.clear_cancel(workspace_id);

    let result = match exec_res {
        Ok(r) => r,
        Err(ContainerRuntimeError::Cancelled) => {
            warn!(
                workspace = workspace_id,
                hook = label,
                "lifecycle hook cancelled by user"
            );
            sink.log(
                workspace_id,
                LogStreamKind::System,
                &format!("{label} cancelled"),
            );
            return Err(LifecycleError::HookCancelled { label });
        }
        Err(source) => {
            return Err(LifecycleError::Stage {
                stage: "hook",
                source,
            })
        }
    };
    if result.exit_code != 0 {
        let stderr_text = String::from_utf8_lossy(&result.stderr);
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

/// Spawn a lifecycle hook detached on the tokio runtime so the
/// orchestrator can return control even when the hook never exits
/// (e.g. `postStartCommand: "jupyter lab"`). Output continues to
/// stream into the host's UI via the cloned sink for as long as the
/// hook runs, and the eventual exit code is reported the same way.
fn spawn_detached_hook(
    sink: Arc<dyn EventSink>,
    runtime: Arc<dyn ContainerRuntime>,
    workspace_id: &str,
    container_id: &str,
    spec: &crate::container::ContainerSpec,
    label: &'static str,
    cmd: &LifecycleCommand,
) {
    let argv = cmd.to_argv();
    if argv.is_empty() {
        return;
    }
    sink.log(
        workspace_id,
        LogStreamKind::System,
        &format!(
            "running {label} in background: {}  (orchestrator does not await long-lived hooks)",
            argv.join(" ")
        ),
    );
    let workspace_id = workspace_id.to_string();
    let container_id = container_id.to_string();
    // Spawn a relay task that fans streamed lines from the runtime
    // exec into the EventSink. Detached hooks are typically
    // long-lived (e.g. `jupyter lab`), so without streaming the user
    // sees nothing until the process exits — which is never.
    let (tx, mut rx) = mpsc::channel::<LogChunk>(256);
    let relay_workspace = workspace_id.clone();
    let relay_sink = sink.clone();
    let relay = tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if !chunk.line.is_empty() {
                relay_sink.log(&relay_workspace, chunk.stream, &chunk.line);
            }
        }
    });
    let opts = ExecOptions {
        command: argv,
        workdir: spec.workdir.clone(),
        env: spec.env.clone(),
        user: spec.user.clone(),
        tty: false,
        log_sink: Some(tx),
        cancel: None,
    };
    tokio::spawn(async move {
        info!(
            workspace = %workspace_id,
            container = %container_id,
            hook = label,
            "running detached lifecycle hook"
        );
        let exec_res = runtime.exec(&container_id, &opts).await;
        // Drop `opts` to close the streaming sender so the relay
        // task drains and exits.
        drop(opts);
        let _ = relay.await;
        match exec_res {
            Ok(result) => {
                if result.exit_code != 0 {
                    let stderr_text = String::from_utf8_lossy(&result.stderr);
                    let mut tail = stderr_text.trim().to_string();
                    if tail.len() > 1024 {
                        let start = tail.len() - 1024;
                        tail = format!("…{}", &tail[start..]);
                    }
                    warn!(
                        workspace = %workspace_id,
                        hook = label,
                        exit_code = result.exit_code,
                        stderr = %tail,
                        "detached lifecycle hook exited non-zero"
                    );
                    sink.log(
                        &workspace_id,
                        LogStreamKind::System,
                        &format!("{label} exited with code {}", result.exit_code),
                    );
                } else {
                    info!(
                        workspace = %workspace_id,
                        hook = label,
                        "detached lifecycle hook exited cleanly"
                    );
                    sink.log(
                        &workspace_id,
                        LogStreamKind::System,
                        &format!("{label} finished"),
                    );
                }
            }
            Err(err) => {
                warn!(
                    workspace = %workspace_id,
                    hook = label,
                    error = %err,
                    "detached lifecycle hook failed to launch"
                );
                sink.log(
                    &workspace_id,
                    LogStreamKind::System,
                    &format!("{label} failed to launch: {err}"),
                );
            }
        }
    });
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
    fn merge_proxy_build_args_picks_up_uppercase_and_lowercase() {
        let env = |n: &str| -> Option<String> {
            match n {
                "HTTPS_PROXY" => Some("http://localhost:3128".into()),
                "http_proxy" => Some("http://127.0.0.1:3128".into()),
                "no_proxy" => Some("localhost,host.docker.internal".into()),
                _ => None,
            }
        };
        let merged = merge_proxy_build_args(HashMap::new(), env);
        assert_eq!(
            merged.get("HTTPS_PROXY").map(String::as_str),
            Some("http://192.168.64.1:3128")
        );
        assert_eq!(
            merged.get("HTTP_PROXY").map(String::as_str),
            Some("http://192.168.64.1:3128")
        );
        // NO_PROXY is a comma-separated list, not a URL — left as-is.
        assert_eq!(
            merged.get("NO_PROXY").map(String::as_str),
            Some("localhost,host.docker.internal")
        );
    }

    #[test]
    fn merge_proxy_build_args_does_not_overwrite_existing() {
        let mut existing = HashMap::new();
        existing.insert(
            "HTTPS_PROXY".to_string(),
            "http://my-proxy:8080".to_string(),
        );
        let merged = merge_proxy_build_args(existing, |_| Some("http://from-env:3128".into()));
        assert_eq!(
            merged.get("HTTPS_PROXY").map(String::as_str),
            Some("http://my-proxy:8080")
        );
    }

    #[test]
    fn merge_proxy_build_args_skips_empty_values() {
        let merged = merge_proxy_build_args(HashMap::new(), |_| Some(String::new()));
        assert!(merged.is_empty());
    }

    #[test]
    fn rewrite_localhost_handles_userinfo_and_paths() {
        assert_eq!(
            rewrite_localhost_to_host_gateway("http://user:pass@localhost:3128/path?q=1"),
            "http://user:pass@192.168.64.1:3128/path?q=1"
        );
        assert_eq!(
            rewrite_localhost_to_host_gateway("http://proxy.corp:8080"),
            "http://proxy.corp:8080"
        );
    }

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
        create_calls: PlMutex<Vec<crate::container::ContainerSpec>>,
    }

    impl FakeRuntime {
        fn new(script: FakeScript) -> Self {
            Self {
                script: PlMutex::new(script),
                build_calls: PlMutex::new(Vec::new()),
                create_calls: PlMutex::new(Vec::new()),
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
            spec: &crate::container::ContainerSpec,
        ) -> Result<String, ContainerRuntimeError> {
            self.create_calls.lock().push(spec.clone());
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
        let sink = Arc::new(CapturingSink::default());

        let status = o
            .up_with_sink(sink.clone(), &registry, "ws", &PathBuf::from("/tmp/ws"))
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
        let sink = Arc::new(CapturingSink::default());

        let err = o
            .up_with_sink(sink.clone(), &registry, "ws", &PathBuf::from("/tmp/ws"))
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
        let sink = Arc::new(CapturingSink::default());

        let status = o
            .rebuild_with_sink(sink.clone(), &registry, "ws", &PathBuf::from("/tmp/ws"))
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
        let sink = Arc::new(CapturingSink::default());

        let status = o
            .up_with_sink(
                sink.clone(),
                &registry,
                "ws",
                &PathBuf::from("/tmp/take-two"),
            )
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
        let sink = Arc::new(CapturingSink::default());

        let err = o
            .up_with_sink(sink.clone(), &registry, "ws", &PathBuf::from("/tmp/ws"))
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
        let sink = Arc::new(CapturingSink::default());

        let err = o
            .up_with_sink(sink.clone(), &registry, "ws", &PathBuf::from("/tmp/ws"))
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
        std::fs::File::create(&cfg)
            .unwrap()
            .write_all(b"{}")
            .unwrap();
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

    // -------- proxy: pure precedence/rewriting logic --------

    #[test]
    fn compute_effective_proxy_env_uses_internal_when_host_unset() {
        let env = compute_effective_proxy_env(|_| None, Some("http://192.168.64.1:31280".into()));
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://192.168.64.1:31280")
        );
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://192.168.64.1:31280")
        );
        assert_eq!(
            env.get("NO_PROXY").map(String::as_str),
            Some(INTERNAL_PROXY_NO_PROXY)
        );
        // Lowercase variants are emitted alongside uppercase so
        // curl / wget / pip / git see them too.
        assert_eq!(
            env.get("http_proxy").map(String::as_str),
            Some("http://192.168.64.1:31280")
        );
        assert_eq!(
            env.get("https_proxy").map(String::as_str),
            Some("http://192.168.64.1:31280")
        );
        assert_eq!(
            env.get("no_proxy").map(String::as_str),
            Some(INTERNAL_PROXY_NO_PROXY)
        );
    }

    #[test]
    fn compute_effective_proxy_env_prefers_host_proxy_over_internal() {
        let host: HashMap<&str, &str> = [("HTTP_PROXY", "http://corp.proxy:8080")].into();
        let env = compute_effective_proxy_env(
            |n| host.get(n).map(|s| s.to_string()),
            Some("http://192.168.64.1:31280".into()),
        );
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://corp.proxy:8080"),
            "host-supplied proxy must win over internal"
        );
        // Lowercase mirror is also emitted.
        assert_eq!(
            env.get("http_proxy").map(String::as_str),
            Some("http://corp.proxy:8080")
        );
        // Internal NO_PROXY default is NOT injected when host wins.
        assert!(!env.contains_key("NO_PROXY"));
        assert!(!env.contains_key("no_proxy"));
    }

    #[test]
    fn compute_effective_proxy_env_rewrites_loopback_in_host_value() {
        let host: HashMap<&str, &str> = [("HTTP_PROXY", "http://localhost:3128")].into();
        let env = compute_effective_proxy_env(|n| host.get(n).map(|s| s.to_string()), None);
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://192.168.64.1:3128"),
            "loopback must be rewritten to bridge gateway IP"
        );
    }

    #[test]
    fn compute_effective_proxy_env_empty_when_host_unset_and_internal_disabled() {
        let env = compute_effective_proxy_env(|_| None, None);
        assert!(env.is_empty());
    }

    #[test]
    fn compute_effective_proxy_env_falls_back_to_lowercase_host_var() {
        let host: HashMap<&str, &str> = [("http_proxy", "http://corp:8080")].into();
        let env = compute_effective_proxy_env(
            |n| host.get(n).map(|s| s.to_string()),
            Some("http://192.168.64.1:31280".into()),
        );
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            Some("http://corp:8080")
        );
    }

    // -------- proxy: end-to-end through the orchestrator --------

    /// Internal proxy URL is auto-injected into both build args and
    /// runtime container env when the host has no proxy set.
    /// Exercised against a real `ProxyManager` bound on an ephemeral
    /// loopback port.
    #[tokio::test]
    async fn up_with_internal_proxy_injects_env_into_build_args_and_container_env() {
        // Bind the proxy on an ephemeral loopback port so the test
        // is independent of whether the Apple Containers bridge is
        // up. Port 0 → kernel chooses a free port → manager records
        // the actual URL we'll see in the spec.
        let proxy = crate::devcontainer::proxy_manager::ProxyManager::with_bind(
            "127.0.0.1:0".parse().unwrap(),
        );
        let o = LifecycleOrchestrator::with_proxy(proxy);
        // Force an empty host env so the internal proxy is the only
        // source. Avoids races on process-global env.
        o.set_host_env_override(HashMap::new());

        // Build-based config so we exercise both code paths.
        let cfg_dir = tempdir();
        std::fs::create_dir_all(cfg_dir.join(".devcontainer")).unwrap();
        let cfg_path = cfg_dir.join(".devcontainer/devcontainer.json");
        std::fs::write(&cfg_path, b"{}").unwrap();
        // Dockerfile content doesn't matter — FakeRuntime never reads it.
        std::fs::write(cfg_dir.join(".devcontainer/Dockerfile"), b"FROM alpine\n").unwrap();
        let parsed = ParsedDevContainer {
            build: Some(DevContainerBuild {
                dockerfile: Some("Dockerfile".into()),
                context: Some(".".into()),
                ..Default::default()
            }),
            config_file_path: Some(cfg_path),
            ..Default::default()
        };
        o.set_parsed_config("ws", parsed);

        let runtime = Arc::new(FakeRuntime::new(FakeScript::default()));
        let registry = RuntimeRegistry::with_single(
            RuntimeId::AppleContainers,
            runtime.clone() as Arc<dyn ContainerRuntime>,
        );
        let sink = Arc::new(CapturingSink::default());

        let status = o
            .up_with_sink(sink, &registry, "ws", &cfg_dir)
            .await
            .expect("up should succeed");
        assert_eq!(status.state, "running");

        // Discover the URL the proxy actually bound on.
        let proxy_url = o
            .proxy()
            .url()
            .expect("proxy should have started lazily during up");
        assert!(proxy_url.starts_with("http://127.0.0.1:"));
        // The build/container injection path applies the standard
        // loopback→bridge-IP rewrite — `127.0.0.1` is unreachable
        // from inside a container, so the URL we expect to see in
        // the spec swaps the host literal but keeps the port.
        let expected_in_spec = proxy_url.replace("127.0.0.1", "192.168.64.1");

        // BuildSpec.build_args carries the (rewritten) proxy.
        let builds = runtime.build_calls.lock().clone();
        assert_eq!(builds.len(), 1);
        let bs = &builds[0];
        assert_eq!(bs.build_args.get("HTTP_PROXY"), Some(&expected_in_spec));
        assert_eq!(bs.build_args.get("HTTPS_PROXY"), Some(&expected_in_spec));
        assert_eq!(
            bs.build_args.get("NO_PROXY").map(String::as_str),
            Some(INTERNAL_PROXY_NO_PROXY)
        );

        // ContainerSpec.env carries the same (rewritten) proxy in
        // both the uppercase form (BuildKit / most modern tools) and
        // the lowercase form (curl, wget, pip, git, plain shell
        // scripts).
        let creates = runtime.create_calls.lock().clone();
        assert_eq!(creates.len(), 1);
        let cs = &creates[0];
        assert_eq!(cs.env.get("HTTP_PROXY"), Some(&expected_in_spec));
        assert_eq!(cs.env.get("HTTPS_PROXY"), Some(&expected_in_spec));
        assert_eq!(cs.env.get("http_proxy"), Some(&expected_in_spec));
        assert_eq!(cs.env.get("https_proxy"), Some(&expected_in_spec));
        assert_eq!(
            cs.env.get("NO_PROXY").map(String::as_str),
            Some(INTERNAL_PROXY_NO_PROXY)
        );
        assert_eq!(
            cs.env.get("no_proxy").map(String::as_str),
            Some(INTERNAL_PROXY_NO_PROXY)
        );
    }

    /// When the host already exports `HTTP_PROXY`, the internal
    /// proxy stays dormant and the host value flows through (with
    /// loopback rewriting) into both build args and container env.
    #[tokio::test]
    async fn up_with_host_proxy_set_does_not_start_internal_proxy() {
        let proxy = crate::devcontainer::proxy_manager::ProxyManager::with_bind(
            "127.0.0.1:0".parse().unwrap(),
        );
        let o = LifecycleOrchestrator::with_proxy(proxy);
        let mut host = HashMap::new();
        host.insert("HTTP_PROXY".into(), "http://localhost:3128".into());
        host.insert("HTTPS_PROXY".into(), "http://localhost:3128".into());
        o.set_host_env_override(host);

        o.set_parsed_config("ws", parsed_with_image("ubuntu:24.04"));
        let runtime = Arc::new(FakeRuntime::new(FakeScript::default()));
        let registry = RuntimeRegistry::with_single(
            RuntimeId::AppleContainers,
            runtime.clone() as Arc<dyn ContainerRuntime>,
        );
        let sink = Arc::new(CapturingSink::default());

        o.up_with_sink(sink, &registry, "ws", &PathBuf::from("/tmp/ws"))
            .await
            .expect("up should succeed");

        // Internal proxy should never have bound — host satisfied
        // the requirement.
        assert!(
            o.proxy().url().is_none(),
            "internal proxy must stay dormant when host provides one"
        );

        // Container env carries the rewritten host value in both
        // cases — `curl`/`pip`/etc. need the lowercase form too.
        let creates = runtime.create_calls.lock().clone();
        assert_eq!(creates.len(), 1);
        let cs = &creates[0];
        assert_eq!(
            cs.env.get("HTTP_PROXY").map(String::as_str),
            Some("http://192.168.64.1:3128"),
            "loopback in host proxy must be rewritten to bridge IP"
        );
        assert_eq!(
            cs.env.get("http_proxy").map(String::as_str),
            Some("http://192.168.64.1:3128"),
            "lowercase variant must be injected for curl/pip/etc."
        );
    }

    /// End-to-end: orchestrator starts the proxy, an external
    /// CONNECT round-trip succeeds, and stats reflect the traffic.
    /// This is the "live integration" test for Phase 1: no
    /// containers, but every layer of our proxy stack is exercised.
    #[tokio::test]
    async fn proxy_started_by_orchestrator_round_trips_connect_and_records_stats() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Tiny in-test "upstream": echoes a known banner then drains.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                let _ = s.write_all(b"HELLO\n").await;
                let mut buf = [0u8; 64];
                let _ = s.read(&mut buf).await;
                let _ = s.shutdown().await;
            }
        });

        // Orchestrator wired to a real proxy on an ephemeral port.
        let proxy = crate::devcontainer::proxy_manager::ProxyManager::with_bind(
            "127.0.0.1:0".parse().unwrap(),
        );
        let o = LifecycleOrchestrator::with_proxy(proxy);
        o.set_host_env_override(HashMap::new());

        // Trigger the lazy bind via the same path `up` would use.
        // The injected env URL gets the loopback→bridge rewrite, so
        // we read the real listener address from the manager rather
        // than from the env map (the rewritten URL is intentionally
        // unreachable from this test process).
        let env = o.effective_proxy_env().await;
        assert_eq!(
            env.get("HTTP_PROXY").map(String::as_str),
            env.get("http_proxy").map(String::as_str),
            "upper- and lowercase env vars must agree"
        );
        let proxy_addr = o
            .proxy()
            .url()
            .expect("proxy URL injected")
            .strip_prefix("http://")
            .unwrap()
            .parse::<std::net::SocketAddr>()
            .unwrap();

        // Manually CONNECT through the proxy to the upstream.
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let req = format!(
            "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n",
            host = upstream_addr.ip(),
            port = upstream_addr.port(),
        );
        client.write_all(req.as_bytes()).await.unwrap();

        // Read the proxy's "200 Connection established" + headers.
        let mut head = Vec::with_capacity(128);
        let mut tmp = [0u8; 256];
        loop {
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0, "proxy closed before sending response");
            head.extend_from_slice(&tmp[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head_str = String::from_utf8_lossy(&head);
        assert!(
            head_str.contains("200"),
            "expected 200 from proxy, got: {head_str}"
        );

        // The bytes after the headers should include the upstream
        // banner. Read more if we haven't seen it yet.
        let split = head
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("found end-of-headers above");
        let mut tail: Vec<u8> = head[split + 4..].to_vec();
        while !tail.windows(b"HELLO".len()).any(|w| w == b"HELLO") {
            let n = client.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&tmp[..n]);
        }
        assert!(
            tail.windows(b"HELLO".len()).any(|w| w == b"HELLO"),
            "expected upstream banner through tunnel; got: {:?}",
            String::from_utf8_lossy(&tail)
        );

        // Tear down so the tunnel completes and stats settle.
        let _ = client.shutdown().await;
        // Give the proxy's bridge task a moment to flush stats.
        for _ in 0..50 {
            if let Some(snap) = o.proxy().stats() {
                if snap.total_connections >= 1 && snap.total_bytes_down >= 1 {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let snap = o.proxy().stats().expect("proxy running");
        assert!(
            snap.total_connections >= 1,
            "expected ≥1 connection in stats, got {snap:?}"
        );
        assert!(
            snap.total_bytes_down >= b"HELLO\n".len() as u64,
            "expected upstream→client bytes counted, got {snap:?}"
        );
        let host_key = upstream_addr.ip().to_string();
        let per_host = snap
            .per_host
            .iter()
            .find(|h| h.host == host_key)
            .unwrap_or_else(|| panic!("missing per-host stats for {host_key}; got {snap:?}"));
        assert!(per_host.connections >= 1);
    }
}
