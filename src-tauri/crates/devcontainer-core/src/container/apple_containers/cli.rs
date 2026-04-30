//! `container` CLI surface used by [`super::AppleContainersRuntime`].
//!
//! Building the argv vectors as pure functions keeps the wire format
//! testable without a real `container` binary on `$PATH`.

use tokio::process::Command;

use crate::container::traits::{
    BuildSpec, ContainerRuntimeError, ContainerSpec, ContainerStatus, ExecOptions, ImageRef,
    LogOptions,
};

use super::{mount_flag, InspectShape};

#[derive(Debug, Clone)]
pub struct ContainerCli {
    binary: String,
}

impl ContainerCli {
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

impl Default for ContainerCli {
    fn default() -> Self {
        Self::new("container")
    }
}

/// Format an [`ImageRef`] back into the canonical `[registry/]repo[:tag][@digest]`
/// string accepted by `container image pull`.
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
    vec![
        "image".to_string(),
        "pull".to_string(),
        image_ref_to_string(image),
    ]
}

/// Argv for `container build --tag <ref> --file <dockerfile> [--build-arg k=v]... [--target T] <context>`.
/// Apple's `container` CLI mirrors Docker/Podman's flags here. Build
/// args are sorted so the argv is deterministic for tests.
pub(crate) fn build_args(spec: &BuildSpec) -> Vec<String> {
    build_args_with_dns(spec, &host_dns_servers())
}

/// Same as [`build_args`] but with explicit DNS servers, so unit tests
/// can pin the argv without depending on the host's resolver state.
pub(crate) fn build_args_with_dns(spec: &BuildSpec, dns: &[String]) -> Vec<String> {
    let mut a = vec![
        "build".to_string(),
        "--tag".to_string(),
        image_ref_to_string(&spec.tag),
        "--file".to_string(),
        spec.dockerfile.display().to_string(),
    ];
    let mut args: Vec<(&String, &String)> = spec.build_args.iter().collect();
    args.sort_by(|x, y| x.0.cmp(y.0));
    for (k, v) in args {
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
    // Prepend the host's resolvers as `--dns` flags so the build VM
    // does not start with an empty resolver list. Apple's buildkit
    // sandbox otherwise inherits no DNS, which surfaces as the
    // "Could not resolve host: github.com" failures users see when a
    // Dockerfile RUN step does an HTTP fetch. Each entry is passed
    // verbatim; if the host has no resolvers configured we simply
    // omit the flags and let the runtime's defaults apply.
    for ip in dns {
        a.push("--dns".to_string());
        a.push(ip.clone());
    }
    a.push(spec.context_dir.display().to_string());
    a
}

/// Discover the host's currently-active DNS resolver IPs so we can
/// hand them to `container build --dns ...`. macOS does not expose its
/// DNS *cache* (mDNSResponder is opaque), but the *resolvers* it is
/// configured with are reachable via `scutil --dns`. Reusing them in
/// the build sandbox means the build hits the same upstream resolver
/// the host is already using — typically the user's router or ISP,
/// which has its own cache — instead of relying on whatever empty
/// default the buildkit VM ships with.
///
/// Returns an empty Vec on any failure so the caller can simply omit
/// the `--dns` flags. Order is preserved (primary resolver first) and
/// duplicates are removed. We cap at 3 to mirror the typical
/// `/etc/resolv.conf` limit; passing a hundred resolvers serves no
/// purpose.
fn host_dns_servers() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let out = match std::process::Command::new("scutil").arg("--dns").output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out);
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    for line in text.lines() {
        // Format: `  nameserver[0] : 192.168.1.1`
        let trimmed = line.trim();
        if !trimmed.starts_with("nameserver") {
            continue;
        }
        let Some((_, ip)) = trimmed.split_once(':') else {
            continue;
        };
        let ip = ip.trim();
        if ip.is_empty() || !looks_like_ip(ip) {
            continue;
        }
        if seen.insert(ip.to_string()) {
            servers.push(ip.to_string());
            if servers.len() >= 3 {
                break;
            }
        }
    }
    servers
}

/// Cheap sanity check: accept anything that parses as an IPv4 or IPv6
/// address. We're not validating reachability — the runtime will
/// surface its own error if the address is unusable.
fn looks_like_ip(s: &str) -> bool {
    s.parse::<std::net::IpAddr>().is_ok()
}

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
    // Sort so argv is deterministic regardless of HashMap iteration order.
    sort_kv_block(&mut a, "--env");

    for m in &spec.mounts {
        a.push("--mount".to_string());
        a.push(mount_flag(m));
    }

    for p in &spec.ports {
        a.push("--publish".to_string());
        let proto = match p.protocol {
            crate::container::PortProtocol::Tcp => "tcp",
            crate::container::PortProtocol::Udp => "udp",
        };
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

    // Verbatim devcontainer.json `runArgs`. Inserted immediately
    // before the image so they behave like `docker run` flags. We
    // do not interpret or filter them — Apple's `container create`
    // accepts a subset (`--cpus`, `--memory`, etc.) and will surface
    // its own error for anything it does not understand.
    for raw in &spec.run_args {
        a.push(raw.clone());
    }

    a.push(image_ref_to_string(&spec.image));
    if let Some(cmd) = &spec.command {
        for arg in cmd {
            a.push(arg.clone());
        }
    }
    a
}

/// Sort consecutive `flag value` pairs in-place so the argv we emit is
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
            // `end` now points at one past the last value in the block.
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

pub(crate) fn remove_args(container_id: &str, force: bool) -> Vec<String> {
    let mut a = vec!["delete".to_string()];
    if force {
        a.push("--force".to_string());
    }
    a.push(container_id.to_string());
    a
}

pub(crate) fn exec_args(container_id: &str, options: &ExecOptions) -> Vec<String> {
    let mut a = vec!["exec".to_string()];
    if options.tty {
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
    // NOTE: Apple's `container exec` takes process arguments positionally
    // straight after the container id; it does NOT accept a `--`
    // separator (it would be interpreted as the executable name and
    // fail with `failed to find target executable --`).
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

pub(crate) fn parse_inspect(
    stdout: &str,
    container_id: &str,
) -> Result<ContainerStatus, ContainerRuntimeError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(ContainerRuntimeError::Backend(
            "`container inspect` returned empty output".into(),
        ));
    }
    // Apple's CLI sometimes wraps a single object in a one-element array.
    if let Ok(arr) = serde_json::from_str::<Vec<InspectShape>>(trimmed) {
        if let Some(first) = arr.into_iter().next() {
            return Ok(first.into_status());
        }
        // Apple's `container inspect <unknown>` exits 0 with `[]` instead
        // of erroring. Surface this as a recognisable "not found" so the
        // orchestrator's adoption / remove-on-rebuild paths can branch
        // on it via `is_not_found`.
        return Err(ContainerRuntimeError::Backend(format!(
            "container with ID {container_id} not found"
        )));
    }
    let one: InspectShape = serde_json::from_str(trimmed).map_err(|e| {
        ContainerRuntimeError::Backend(format!("could not parse `container inspect`: {e}"))
    })?;
    Ok(one.into_status())
}

pub(crate) fn parse_list(stdout: &str) -> Result<Vec<ContainerStatus>, ContainerRuntimeError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let arr: Vec<InspectShape> = serde_json::from_str(trimmed).map_err(|e| {
        ContainerRuntimeError::Backend(format!("could not parse `container list`: {e}"))
    })?;
    Ok(arr.into_iter().map(InspectShape::into_status).collect())
}

/// Parse the textual output of `container system status` and decide
/// whether the daemon is up. The CLI prints a key/value table whose
/// first data row is `status running` or `status stopped`. Any
/// unrecognised output is treated as "not running" so the caller will
/// attempt a start (which is idempotent).
pub(crate) fn system_status_is_running(stdout: &str) -> bool {
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("status") {
            let value = rest.trim();
            if value.eq_ignore_ascii_case("running") {
                return true;
            }
            return false;
        }
    }
    false
}

/// Whether `container image list --format json` contains an entry whose
/// `reference` matches `needle` (a fully-formed image reference such as
/// `devcontainer-foo:latest`). Robust against missing fields and
/// non-JSON output (returns false).
///
/// Match is permissive on the registry prefix: Apple's CLI reports
/// Docker Hub images with the implicit `docker.io/library/` prefix
/// expanded (`docker.io/library/alpine:3.19`), even when they were
/// pulled by the bare `library/alpine:3.19` ref. So we accept either
/// an exact match or a suffix match against `/<needle>` so the cache
/// hit fires regardless of how the caller spelled the reference.
pub(crate) fn image_list_contains(stdout: &str, needle: &str) -> bool {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) else {
        return false;
    };
    arr.iter().any(|v| {
        v.get("reference")
            .and_then(|r| r.as_str())
            .map(|s| s == needle || s.ends_with(&format!("/{needle}")))
            .unwrap_or(false)
    })
}

/// Whether `container system dns ls` mentions `domain` somewhere in its
/// output. The CLI emits a small text table whose first column is the
/// domain name; we don't need to fully parse it because the domain
/// strings we care about (`host.docker.internal`) are unique enough to
/// match by substring without false positives.
pub(crate) fn dns_list_contains(stdout: &str, domain: &str) -> bool {
    stdout.lines().any(|line| {
        line.split_whitespace()
            .next()
            .map(|first| first.eq_ignore_ascii_case(domain))
            .unwrap_or(false)
    })
}

/// Parse the localhost-redirect IP recorded in an `/etc/resolver/`
/// file written by `container system dns create <domain> --localhost
/// <ip>`. The file format is a small set of `key value` lines; the IP
/// lives on a line of the form `options localhost:<ip>`. Returns
/// `None` if the marker line is absent or malformed.
pub(crate) fn parse_resolver_localhost_ip(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        let rest = match line.strip_prefix("options") {
            Some(r) => r.trim(),
            None => continue,
        };
        // `options` may carry multiple space-separated entries; find
        // the one starting with `localhost:`.
        for token in rest.split_whitespace() {
            if let Some(ip) = token.strip_prefix("localhost:") {
                let ip = ip.trim();
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

/// Read a config-time label from `container image inspect <ref>` output.
/// Apple's CLI returns an array of image entries, each with one or more
/// `variants[].config.config.Labels` maps. We scan all variants so a
/// platform-specific build is found regardless of host architecture.
pub(crate) fn image_label_from_inspect(stdout: &str, key: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let arr: Vec<serde_json::Value> = serde_json::from_str(trimmed).ok()?;
    for entry in arr {
        let variants = entry.get("variants")?.as_array()?.clone();
        for v in variants {
            let labels = v
                .get("config")
                .and_then(|c| c.get("config"))
                .and_then(|c| c.get("Labels"));
            if let Some(map) = labels.and_then(|l| l.as_object()) {
                if let Some(val) = map.get(key).and_then(|s| s.as_str()) {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}
