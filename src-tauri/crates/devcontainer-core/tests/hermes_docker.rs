//! End-to-end acceptance test: `wiki3-ai/hermes-devcontainer` on Docker.
//!
//! Ignored by default. It needs a running Docker daemon, the Hermes project
//! checked out locally, and the `nousresearch/hermes-agent` base image
//! (~3.9 GB) present or pullable. That is not something CI should do.
//!
//! ```text
//! cargo test -p devcontainer-core --test hermes_docker -- --ignored --nocapture
//! ```
//!
//! Override the project location with `WIKI3_HERMES_DEVCONTAINER=/path/to/repo`.
//!
//! This is the generic Dev Container contract, exercised against a real
//! project: custom Dockerfile, `overrideCommand: false` preserving the
//! image's own `CMD`, a named volume for durable state, `runArgs` for host
//! aliases, `containerEnv`, forwarded ports and a `postCreateCommand`.
//! Nothing here is special-cased for Hermes in the implementation under
//! test — the project is the fixture, not the subject.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use devcontainer_core::{
    DockerRuntime, EventSink, LifecycleOrchestrator, LogStreamKind, ParsedDevContainer, RuntimeId,
    RuntimeRegistry,
};

/// Prints orchestrator events so `--nocapture` shows the real build log.
struct PrintSink;

impl EventSink for PrintSink {
    fn status(
        &self,
        workspace_id: &str,
        state: &str,
        container_id: Option<&str>,
        image_ref: Option<&str>,
        error: Option<&str>,
    ) {
        println!(
            "[status] {workspace_id}: {state} \
             (container={container_id:?} image={image_ref:?} error={error:?})"
        );
    }

    fn log(&self, _workspace_id: &str, stream: LogStreamKind, line: &str) {
        println!("[{stream:?}] {line}");
    }
}

fn hermes_repo() -> Option<PathBuf> {
    let path = std::env::var("WIKI3_HERMES_DEVCONTAINER")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join("Projects/hermes-devcontainer"))
        })?;
    if path.join(".devcontainer/devcontainer.json").is_file() {
        Some(path)
    } else {
        None
    }
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `docker …` and return `(success, stdout)`.
fn docker(args: &[&str]) -> (bool, String) {
    match Command::new("docker").args(args).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(e) => (false, format!("failed to run docker: {e}")),
    }
}

/// Poll a TCP port with a minimal HTTP/1.0 request until it answers.
///
/// Deliberately hand-rolled: the port may well reply `401` (the Hermes
/// dashboard is behind basic auth) and "the socket answered with a real
/// HTTP response" is the property under test, not the status code. Adding
/// an HTTP client dependency to the engine crate for this would be silly.
fn http_responds(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
            let request =
                format!("GET / HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut buf = [0u8; 512];
                if let Ok(n) = stream.read(&mut buf) {
                    if n > 0 {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Read the real `devcontainer.json` the way the WebView engine would.
///
/// The engine bundle parses JSONC and then projects it onto
/// `ParsedDevContainer`; here the file is plain JSON, so serde does the
/// projection directly. `configFilePath` is injected because the bundle
/// adds it (the orchestrator resolves `build.dockerfile` relative to it).
fn load_parsed(repo: &Path) -> ParsedDevContainer {
    let cfg_path = repo.join(".devcontainer/devcontainer.json");
    let text = std::fs::read_to_string(&cfg_path).expect("read devcontainer.json");
    let mut parsed: ParsedDevContainer =
        serde_json::from_str(&text).expect("parse devcontainer.json into ParsedDevContainer");
    parsed.config_file_path = Some(cfg_path.clone());
    parsed
}

fn assert_prerequisites(repo: &Path) -> bool {
    if !docker_available() {
        eprintln!("SKIP: Docker daemon not reachable");
        return false;
    }
    if !repo.join(".devcontainer/devcontainer.json").is_file() {
        eprintln!(
            "SKIP: no .devcontainer/devcontainer.json under {}",
            repo.display()
        );
        return false;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker, the hermes-devcontainer checkout, and a 3.9GB base image"]
async fn hermes_devcontainer_runs_on_docker() {
    let Some(repo) = hermes_repo() else {
        eprintln!("SKIP: hermes-devcontainer not found (set WIKI3_HERMES_DEVCONTAINER)");
        return;
    };
    if !assert_prerequisites(&repo) {
        return;
    }

    let parsed = load_parsed(&repo);

    // --- the parsed contract must actually carry what the project asks for
    assert_eq!(
        parsed.override_command,
        Some(false),
        "overrideCommand:false must survive parsing; otherwise the image CMD is replaced"
    );
    assert_eq!(
        parsed.mounts,
        vec!["source=hermes-opt-data,target=/opt/data,type=volume".to_string()],
        "the named volume must survive parsing"
    );
    assert_eq!(
        parsed.run_args.len(),
        2,
        "both --add-host args must survive"
    );

    let orchestrator = LifecycleOrchestrator::new();
    orchestrator.set_parsed_config("hermes", parsed);
    orchestrator.record_host_workspace("hermes", &repo);

    // Pin Docker explicitly: this test is about the Docker backend, not
    // about the availability policy.
    let registry = RuntimeRegistry::with_single(RuntimeId::Docker, Arc::new(DockerRuntime::new()));
    let sink: Arc<dyn EventSink> = Arc::new(PrintSink);

    // ---- 1. build + create + start + lifecycle hooks all succeed -------
    let status = orchestrator
        .up_with_sink(sink.clone(), &registry, "hermes", &repo)
        .await
        .expect("`up` must succeed on Docker");
    assert_eq!(status.state, "running", "status: {status:?}");
    let cid = status
        .container_id
        .clone()
        .expect("`up` must report a container id");
    println!("\n=== container: {cid} ===");

    // ---- 2. the container is running -----------------------------------
    let (_, running) = docker(&["inspect", "--format", "{{.State.Running}}", &cid]);
    assert_eq!(running, "true", "container should be running");

    // ---- 3. the named volume is mounted at /opt/data -------------------
    let (_, mounts) = docker(&["inspect", "--format", "{{json .Mounts}}", &cid]);
    println!("mounts: {mounts}");
    assert!(
        mounts.contains("\"Name\":\"hermes-opt-data\""),
        "expected the hermes-opt-data volume, got: {mounts}"
    );
    assert!(
        mounts.contains("\"Destination\":\"/opt/data\""),
        "expected it mounted at /opt/data, got: {mounts}"
    );

    // ---- 4. the image's own CMD runs, not our keepalive -----------------
    // With `overrideCommand:false` we must pass no command at all, so the
    // Dockerfile's `CMD ["gateway", "run"]` is what the container runs.
    let (_, cmd) = docker(&["inspect", "--format", "{{json .Config.Cmd}}", &cid]);
    let (_, entrypoint) = docker(&["inspect", "--format", "{{json .Config.Entrypoint}}", &cid]);
    println!("cmd: {cmd}  entrypoint: {entrypoint}");
    assert!(
        !cmd.contains("sleep 2147483647"),
        "the Wiki3 keepalive sleep loop must NOT be used here; got {cmd}"
    );
    assert!(
        cmd.contains("gateway"),
        "the image's CMD (`gateway run`) must survive; got {cmd}"
    );

    // ---- 5. postCreateCommand configured the Unsloth provider ----------
    let (ok, api) = docker(&[
        "exec",
        &cid,
        "hermes",
        "config",
        "get",
        "providers.unsloth.api",
    ]);
    println!("providers.unsloth.api -> ({ok}) {api}");
    assert!(ok, "`hermes config get` must succeed inside the container");
    assert!(
        api.contains("host.docker.internal:8888"),
        "configure-hermes.sh should have set the Unsloth provider URL; got {api}"
    );

    // ---- 6. the dashboard answers on the forwarded port ----------------
    assert!(
        http_responds(9119, Duration::from_secs(90)),
        "the Hermes dashboard should answer on host port 9119 within 90s"
    );
    println!("dashboard responded on http://127.0.0.1:9119/");

    // ---- 7. host-service reachability (opt-in) -------------------------
    // Off by default: it needs Unsloth actually listening on the host.
    if std::env::var("WIKI3_CHECK_UNSLOTH").ok().as_deref() == Some("1") {
        let (ok, out) = docker(&[
            "exec",
            &cid,
            "curl",
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "5",
            "http://host.docker.internal:8888/v1/models",
        ]);
        println!("unsloth reachability -> ({ok}) {out}");
        assert!(
            ok,
            "host.docker.internal:8888 should be reachable from the container"
        );
    } else {
        println!("(skipping Unsloth reachability check; set WIKI3_CHECK_UNSLOTH=1 to enable)");
    }

    println!("\nPASS. Container {cid} left running for inspection.");
    println!("  docker stop {cid} && docker rm {cid}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker and the hermes-devcontainer checkout"]
async fn hermes_data_volume_survives_recreation() {
    let Some(repo) = hermes_repo() else {
        eprintln!("SKIP: hermes-devcontainer not found");
        return;
    };
    if !assert_prerequisites(&repo) {
        return;
    }

    let orchestrator = LifecycleOrchestrator::new();
    orchestrator.set_parsed_config("hermes", load_parsed(&repo));
    orchestrator.record_host_workspace("hermes", &repo);
    let registry = RuntimeRegistry::with_single(RuntimeId::Docker, Arc::new(DockerRuntime::new()));
    let sink: Arc<dyn EventSink> = Arc::new(PrintSink);

    let status = orchestrator
        .up_with_sink(sink.clone(), &registry, "hermes", &repo)
        .await
        .expect("first up");
    let cid = status.container_id.clone().expect("container id");

    // Write a marker inside the durable volume.
    let marker = "/opt/data/wiki3-persistence-marker";
    let (ok, out) = docker(&[
        "exec",
        &cid,
        "sh",
        "-c",
        &format!("echo persisted > {marker} && cat {marker}"),
    ]);
    assert!(ok, "writing the marker should succeed: {out}");

    // Recreate the container. The volume must be adopted, not recreated.
    orchestrator
        .remove_with_sink(sink.as_ref(), &registry, "hermes")
        .await
        .expect("remove");

    let status = orchestrator
        .up_with_sink(sink.clone(), &registry, "hermes", &repo)
        .await
        .expect("second up");
    let cid2 = status
        .container_id
        .clone()
        .expect("container id after recreate");
    assert_ne!(cid, cid2, "recreation should produce a new container");

    let (ok, out) = docker(&["exec", &cid2, "cat", marker]);
    println!("marker after recreate -> ({ok}) {out}");
    assert!(ok, "the marker should survive recreation: {out}");
    assert!(out.contains("persisted"), "marker contents: {out}");

    println!("\nPASS. /opt/data survived container recreation ({cid} -> {cid2}).");
}
