//! Podman smoke test — exercises the shared Docker-compatible CLI surface
//! against a real Podman machine.
//!
//! Ignored by default: needs a running `podman machine` and network access to
//! pull a small base image.
//!
//! ```text
//! cargo test -p devcontainer-core --test podman_smoke -- --ignored --nocapture
//! ```
//!
//! Why this exists: `PodmanRuntime` deliberately drives the Docker backend's
//! argv builders and inspect parsing rather than duplicating them. That
//! sharing is only sound if Podman really does accept the same argv and emit
//! the same JSON. This test checks the claim instead of assuming it — in
//! particular that `podman inspect` parses through `parse_inspect` unchanged,
//! since that is the piece most likely to drift between the two engines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use devcontainer_core::{
    ContainerRuntime, ContainerSpec, ContainerState, ExecOptions, ImageRef, LogOptions, MountKind,
    MountSpec, PodmanRuntime, PortForward, PortProtocol,
};

/// Ask the OS for a port nothing is using, so publishing it cannot collide
/// with another container or a developer's dev server.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("bind ephemeral port")
}

/// Compare a runtime-reported host path with an expected one, tolerating
/// trailing slashes and symlinked prefixes (`/var` vs `/private/var`).
fn same_path(reported: &str, wanted: &Path) -> bool {
    let reported = PathBuf::from(reported);
    if reported == wanted {
        return true;
    }
    match (reported.canonicalize(), wanted.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

async fn exec_sh(rt: &PodmanRuntime, id: &str, script: &str) -> (i32, String, String) {
    let options = ExecOptions {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        workdir: None,
        env: HashMap::new(),
        user: None,
        tty: false,
        log_sink: None,
        cancel: None,
    };
    let out = rt.exec(id, &options).await.expect("exec");
    (
        out.exit_code,
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a running podman machine and network for the image pull"]
async fn podman_round_trips_a_small_container() {
    let rt = PodmanRuntime::new();

    let probe = rt.probe().await.expect("probe");
    if !probe.available {
        eprintln!("SKIP: podman not runnable: {:?}", probe.reason);
        return;
    }
    println!("podman: {:?}", probe.version);
    if let Err(e) = rt.ensure_system_running(None).await {
        eprintln!("SKIP: podman machine not up: {e}");
        return;
    }

    let image = ImageRef {
        registry: None,
        repository: "alpine".into(),
        tag: Some("3.20".into()),
        digest: None,
    };
    rt.pull(&image).await.expect("pull alpine");
    println!("pulled alpine:3.20");

    // A host directory under $HOME rather than the system temp dir: Podman's
    // VM shares /Users verbatim, whereas /var/folders is a symlink whose
    // reported source can differ from the path we handed in.
    let host_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .expect("HOME")
        .join(format!(".wiki3-podman-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&host_dir).expect("create host dir");
    std::fs::write(host_dir.join("marker.txt"), b"hello-from-host").expect("write marker");

    let port = free_port();
    let mut env = HashMap::new();
    env.insert("WIKI3_SMOKE".to_string(), "yes".to_string());
    let mut labels = HashMap::new();
    labels.insert(
        "org.devcontainers.config_hash".to_string(),
        "smoke".to_string(),
    );

    let spec = ContainerSpec {
        name: format!("wiki3-podman-smoke-{port}"),
        image,
        // Something long-lived so exec/logs have a target, echoing on the way
        // up so `logs` has a line to find.
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo container-up; sleep 120".into(),
        ]),
        workdir: Some(PathBuf::from("/work")),
        env,
        mounts: vec![MountSpec {
            kind: MountKind::Bind,
            source: host_dir.clone(),
            target: PathBuf::from("/work"),
            read_only: false,
        }],
        ports: vec![PortForward {
            host_port: port,
            container_port: 80,
            protocol: PortProtocol::Tcp,
        }],
        user: None,
        privileged: false,
        run_args: vec![],
        labels,
    };

    let id = rt.create(&spec).await.expect("create");
    println!("created {id}");
    rt.start(&id).await.expect("start");

    // --- inspect: the shared parser must read Podman's JSON unchanged ------
    let status = rt.inspect(&id).await.expect("inspect");
    println!(
        "inspect -> state={:?} image={:?} mounts={:?} labels={:?}",
        status.state, status.image_ref, status.host_mounts, status.labels
    );
    assert_eq!(status.state, ContainerState::Running);
    // Podman normalises the reference to its fully-qualified form
    // (`docker.io/library/alpine:3.20`) where Docker echoes back exactly what
    // it was given (`alpine:3.20`). The field is informational, so assert on
    // the identity of the image rather than the exact spelling — this is
    // precisely the kind of difference this smoke test exists to surface.
    assert!(
        status
            .image_ref
            .as_deref()
            .is_some_and(|r| r.contains("alpine") && r.contains("3.20")),
        "image_ref came back as {:?}",
        status.image_ref
    );
    assert_eq!(
        status
            .labels
            .get("org.devcontainers.config_hash")
            .map(String::as_str),
        Some("smoke"),
        "labels came back as {:?}",
        status.labels
    );
    // The bind source is what the port poller and dashboard use to link a
    // container back to a repo without relying on a name convention.
    assert!(
        status.host_mounts.iter().any(|m| same_path(m, &host_dir)),
        "expected {} among host_mounts {:?}",
        host_dir.display(),
        status.host_mounts
    );

    // --- exec: the mount and the env both made it in ----------------------
    let (code, out, err) = exec_sh(&rt, &id, "cat /work/marker.txt").await;
    assert_eq!(code, 0, "exec failed: {err}");
    assert_eq!(out.trim(), "hello-from-host");

    let (code, out, err) = exec_sh(&rt, &id, "printf %s \"$WIKI3_SMOKE\"").await;
    assert_eq!(code, 0, "exec failed: {err}");
    // Trailing whitespace is trimmed because the shared exec pump is
    // line-oriented: it reads lines and re-appends the newline, so a command
    // whose stdout has no trailing newline gains one. See `spawn_stream_pump`.
    assert_eq!(out.trim(), "yes", "--env did not reach the container");

    // --- logs: output streams back ---------------------------------------
    let mut stream = rt
        .logs(
            &id,
            &LogOptions {
                follow: false,
                tail: None,
            },
        )
        .await
        .expect("logs");
    let mut saw_startup = false;
    while let Some(chunk) = stream.recv().await {
        if chunk.line.contains("container-up") {
            saw_startup = true;
        }
    }
    assert!(
        saw_startup,
        "expected the container's stdout in `podman logs`"
    );

    // --- list: the stale-container checks depend on this ------------------
    let all = rt.list().await.expect("list");
    let short = &id[..12.min(id.len())];
    assert!(
        all.iter().any(|c| c.container_id.starts_with(short)),
        "list() did not include {id}; got {:?}",
        all.iter().map(|c| &c.container_id).collect::<Vec<_>>()
    );

    // --- remove: gone, not merely stopped --------------------------------
    rt.remove(&id, true).await.expect("remove");
    assert!(
        rt.inspect(&id).await.is_err(),
        "the container must no longer exist after remove"
    );

    let _ = std::fs::remove_dir_all(&host_dir);
    println!("PASS. Podman round-tripped create/start/inspect/exec/logs/list/remove.");
}
