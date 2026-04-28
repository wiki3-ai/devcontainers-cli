//! Integration tests against the real Apple `container` CLI.
//!
//! Gated on the `apple-containers-live` cargo feature *and* a runtime probe
//! that runs `container --version`. If the binary isn't on PATH (e.g. on
//! Linux CI), each test prints a skip message and returns Ok — so these
//! tests are safe to leave enabled in `cargo test --all-targets` while
//! still providing real coverage on macOS 26+ developer machines.
//!
//! Each test uses a uniquely-named container and an RAII cleanup guard so
//! a panic mid-test doesn't leak containers.
//!
//! Run on macOS 26 with the container service active:
//!
//! ```sh
//! cargo test --features apple-containers-live --test apple_containers_real
//! ```

#![cfg(feature = "apple-containers-live")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use devcontainers_app_lib::container::{
    apple_containers::AppleContainersRuntime, ContainerRuntime, ContainerSpec, ContainerState,
    ExecOptions, ImageRef,
};

// ---------------------------------------------------------------------------
// Skip logic + helpers
// ---------------------------------------------------------------------------

/// Returns true when `container --version` succeeds. Used to gracefully
/// skip on hosts without the Apple CLI (Linux CI, older macOS, etc).
fn container_cli_available() -> bool {
    std::process::Command::new("container")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Convenience macro: emit a skip line and return early if the real CLI
/// isn't available. Keeps each test's happy path readable.
macro_rules! require_cli {
    () => {
        if !container_cli_available() {
            eprintln!(
                "skipping {}: real `container` CLI not available on PATH",
                stdfn!()
            );
            return;
        }
    };
}

macro_rules! stdfn {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let n = type_name_of(f);
        &n[..n.len() - 3]
    }};
}

/// Process-unique sequence to disambiguate container names within a single
/// `cargo test` run. Combined with a wall-clock seed per process.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_name(tag: &str) -> String {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("dcc-it-{tag}-{seed}-{n}")
}

/// RAII guard that force-deletes the container when dropped. Run via the
/// real CLI so we still clean up even if our adapter is the source of the
/// bug under test.
struct Cleanup(String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("container")
            .args(["delete", "--force", &self.0])
            .output();
    }
}

fn alpine() -> ImageRef {
    ImageRef {
        registry: None,
        repository: "library/alpine".into(),
        tag: Some("3.19".into()),
        digest: None,
    }
}

/// Spec matching what `to_container_spec` emits for Apple containers: a
/// long-running keep-alive command so `exec` has time to land.
fn keepalive_spec(name: &str) -> ContainerSpec {
    ContainerSpec {
        name: name.into(),
        image: alpine(),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "while sleep 2147483647; do :; done".into(),
        ]),
        workdir: None,
        env: Default::default(),
        mounts: vec![],
        ports: vec![],
        user: None,
        privileged: false,
        run_args: vec![],
    }
}

fn exec_opts(cmd: &[&str]) -> ExecOptions {
    ExecOptions {
        command: cmd.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: Default::default(),
        user: None,
        tty: false,
    }
}

fn rt() -> AppleContainersRuntime {
    AppleContainersRuntime::with_binary("container")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full lifecycle: pull → create → start (must poll until running) →
/// inspect (must NOT pass `--format`, must read nested
/// `configuration.id`/`image`) → list (same) → exec (must NOT pass `--`) →
/// stop → remove.
///
/// This single test covers most of the regressions we shipped:
///   * inspect `--format` rejection.
///   * exec `--` separator rejection.
///   * list/inspect identity nested under `configuration`.
///   * start returning before `running`.
///   * overrideCommand keep-alive (otherwise the container exits before
///     exec lands).
#[tokio::test]
async fn real_full_lifecycle_round_trips() {
    require_cli!();
    let rt = rt();
    let name = unique_name("life");
    let _cleanup = Cleanup(name.clone());

    // pull is idempotent; alpine:3.19 is small and cached after first run.
    rt.pull(&alpine()).await.expect("pull alpine");

    let cid = rt.create(&keepalive_spec(&name)).await.expect("create");
    assert!(!cid.is_empty(), "create must return a non-empty id");

    rt.start(&cid).await.expect("start");

    // inspect: container_id and image_ref must come back populated, which
    // only works if we read `configuration.id` / `configuration.image`.
    let status = rt.inspect(&cid).await.expect("inspect");
    assert_eq!(status.state, ContainerState::Running, "must be running");
    assert!(
        !status.container_id.is_empty(),
        "container_id must be populated (regression: nested configuration)",
    );
    assert!(
        status.image_ref.as_deref().is_some_and(|s| !s.is_empty()),
        "image_ref must be populated (regression: nested configuration), got {:?}",
        status.image_ref,
    );

    // list: every entry must have a non-empty container_id; ours must be
    // present.
    let listed = rt.list().await.expect("list");
    assert!(
        listed.iter().all(|s| !s.container_id.is_empty()),
        "every list entry must have a populated container_id",
    );
    assert!(
        listed.iter().any(|s| s.container_id == cid),
        "freshly-created container must appear in list",
    );

    // exec: regression for `failed to find target executable --`.
    let result = rt
        .exec(&cid, &exec_opts(&["/bin/sh", "-c", "echo hello-real"]))
        .await
        .expect("exec");
    assert_eq!(result.exit_code, 0, "exec exit code");
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("hello-real"), "stdout: {stdout}");

    rt.stop(&cid).await.expect("stop");
    rt.remove(&cid, true).await.expect("remove");
}

/// Creating a container with a name that already exists must return an
/// error that the lifecycle orchestrator can recognise as "already
/// exists" so it can adopt the existing container.
#[tokio::test]
async fn real_create_duplicate_name_errors() {
    require_cli!();
    let rt = rt();
    let name = unique_name("dup");
    let _cleanup = Cleanup(name.clone());
    rt.pull(&alpine()).await.expect("pull alpine");

    let cid = rt
        .create(&keepalive_spec(&name))
        .await
        .expect("first create");
    assert!(!cid.is_empty());

    let err = rt
        .create(&keepalive_spec(&name))
        .await
        .expect_err("second create with same name must fail");
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("already exists") || msg.contains("exists"),
        "expected 'already exists' in error, got: {msg}",
    );
}

/// Inspecting a non-existent container must fail with a recognisable
/// "not found" error so adoption logic can branch on it.
#[tokio::test]
async fn real_inspect_unknown_id_errors_not_found() {
    require_cli!();
    let rt = rt();
    let bogus = unique_name("nope");
    let err = rt
        .inspect(&bogus)
        .await
        .expect_err("inspect of unknown id must fail");
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(
        msg.contains("not found") || msg.contains("no such"),
        "expected 'not found' in error, got: {msg}",
    );
}

/// An empty container id must be rejected by our adapter *before* it
/// shells out; otherwise the user sees a useless
/// `container with ID  not found` message.
#[tokio::test]
async fn real_empty_id_rejected_before_shelling_out() {
    require_cli!();
    let rt = rt();
    for r in [
        rt.start("").await,
        rt.stop("").await,
        rt.remove("", true).await,
        rt.inspect("").await.map(|_| ()),
    ] {
        let err = r.expect_err("empty id must be rejected");
        let msg = format!("{err}").to_ascii_lowercase();
        assert!(
            msg.contains("missing container id"),
            "expected local guard, got: {msg}",
        );
    }
}
