//! Integration tests for the Apple `container` CLI adapter.
//!
//! These tests gate on the `apple-containers-live` cargo feature. They
//! exercise [`AppleContainersRuntime`] end-to-end by pointing it at a
//! tempdir-installed fake `container` binary that *strictly* mimics the
//! real CLI's argv contract — every quirk we've been bitten by lives in
//! the assertions below so a regression fails the build instead of only
//! showing up when the user clicks "Up":
//!
//!   * `inspect` does NOT accept `--format` (always emits JSON).
//!   * `inspect` / `list` emit `{ configuration: { id, image }, status }`.
//!   * `exec` takes process argv positionally — a literal `--` is the
//!     executable name and the CLI errors with "failed to find target
//!     executable --".
//!   * `create` of a duplicate name fails with
//!     `exists: "container already exists: NAME"`.
//!   * `start` returns before the container reaches `running`; `inspect`
//!     must be polled.
//!   * Apple's container exits as soon as its CMD does (no daemon),
//!     so spec translation must inject a long-running command.
//!
//! When the real `container` binary is available on PATH and
//! `DEVCONTAINERS_LIVE_REAL=1` is set, additional tests at the bottom of
//! this file drive the actual CLI through a minimal lifecycle so we
//! also catch upstream behaviour drift.

#![cfg(feature = "apple-containers-live")]

use std::time::Duration;

use devcontainers_app_lib::container::{
    apple_containers::AppleContainersRuntime, ContainerRuntime, ContainerRuntimeError,
    ContainerSpec, ContainerState, ExecOptions, ImageRef, LogOptions,
};

fn exec_opts(cmd: &[&str]) -> ExecOptions {
    ExecOptions {
        command: cmd.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: Default::default(),
        user: None,
        tty: false,
    }
}

// ---------------------------------------------------------------------------
// Strict fake CLI
// ---------------------------------------------------------------------------

/// Materialise a fake `container` binary that mimics the real Apple CLI's
/// argv contract. State (created containers, running flag) lives in a
/// sibling state directory so successive invocations interact like the
/// real CLI does.
fn fake_container_script() -> (std::path::PathBuf, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "devcontainers-fake-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let script = dir.join("container");
    let body = format!(
        r#"#!/usr/bin/env bash
set -eu
state="{state}"

die() {{ echo "Error: $*" 1>&2; exit 1; }}

# Reject empty positional args; the real CLI errors with
# `notFound: "container with ID  not found"`.
require_id() {{
    if [ -z "$1" ]; then
        echo 'Error: internalError: "failed to '"$2"' container" (cause: "notFound: \"container with ID  not found\"")' 1>&2
        exit 1
    fi
}}

cmd="$1"; shift || true
case "$cmd" in
  --version) echo "container 0.0.0-fake" ;;

  image)
    sub="$1"; shift
    case "$sub" in
      pull) echo "pulled $1" ;;
      *) die "unknown image subcommand: $sub" ;;
    esac
    ;;

  create)
    name=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift 2 ;;
        --) shift; break ;;
        *) shift ;;
      esac
    done
    [ -n "$name" ] || die "missing --name"
    if [ -e "$state/c-$name" ]; then
      echo "Error: failed to create container (cause: \"exists: \"container already exists: $name\"\")" 1>&2
      exit 1
    fi
    : > "$state/c-$name"
    echo "$name"
    ;;

  start)
    id="${{1:-}}"; require_id "$id" start
    [ -e "$state/c-$id" ] || die "container with ID $id not found"
    # Real CLI returns *before* the container is fully running; we
    # mimic that by writing a `pending` marker first then a `running`
    # marker on the next inspect to force a poll.
    if [ ! -e "$state/r-$id" ]; then
      : > "$state/p-$id"
    fi
    ;;

  stop)
    id="${{1:-}}"; require_id "$id" stop
    [ -e "$state/c-$id" ] || die "container with ID $id not found"
    rm -f "$state/r-$id" "$state/p-$id"
    : > "$state/s-$id"
    ;;

  delete)
    force=0
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --force) force=1; shift ;;
        *) break ;;
      esac
    done
    id="${{1:-}}"; require_id "$id" delete
    [ -e "$state/c-$id" ] || {{
      echo "Error: internalError: \"failed to delete container\" (cause: \"notFound: \"container with ID $id not found\"\")" 1>&2
      exit 1
    }}
    rm -f "$state/c-$id" "$state/r-$id" "$state/p-$id" "$state/s-$id"
    ;;

  inspect)
    # The real CLI rejects `--format` outright with
    # `Error: Unknown option '--format'` and exit code 64.
    for a in "$@"; do
      case "$a" in
        --format|--format=*)
          echo "Error: Unknown option '--format'" 1>&2
          exit 64 ;;
      esac
    done
    id="${{1:-}}"; require_id "$id" inspect
    [ -e "$state/c-$id" ] || {{
      echo "Error: container with ID $id not found" 1>&2
      exit 1
    }}
    status="created"
    if [ -e "$state/r-$id" ]; then status="running"
    elif [ -e "$state/p-$id" ]; then
      # First poll: not running yet. Second poll: promote to running.
      rm -f "$state/p-$id"; : > "$state/r-$id"
      status="created"
    elif [ -e "$state/s-$id" ]; then status="stopped"
    fi
    # Identity nested under `configuration` like the real CLI.
    printf '[{{"status":"%s","configuration":{{"id":"%s","image":{{"reference":"ubuntu:24.04"}}}}}}]\n' \
      "$status" "$id"
    ;;

  list)
    # Always emit JSON nested under `configuration`.
    printf '['
    first=1
    for f in "$state"/c-*; do
      [ -e "$f" ] || continue
      id="${{f##*/c-}}"
      status="created"
      [ -e "$state/r-$id" ] && status="running"
      [ -e "$state/p-$id" ] && status="created"
      [ -e "$state/s-$id" ] && status="stopped"
      if [ $first -eq 0 ]; then printf ','; fi
      first=0
      printf '{{"status":"%s","configuration":{{"id":"%s","image":{{"reference":"ubuntu:24.04"}}}}}}' "$status" "$id"
    done
    printf ']\n'
    ;;

  exec)
    # Mirror Apple's argv parsing: options, then container-id, then
    # process argv positionally. A literal `--` after the id is
    # treated as the executable and the real CLI errors with
    # `failed to find target executable --`.
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --workdir|--cwd|-w|--env|-e|--user|-u|--uid|--gid|--ulimit|--env-file)
          shift 2 ;;
        --tty|-t|--interactive|-i|--detach|-d|--debug)
          shift ;;
        *) break ;;
      esac
    done
    id="${{1:-}}"; require_id "$id" exec; shift
    [ -e "$state/c-$id" ] || die "container with ID $id not found"
    [ -e "$state/r-$id" ] || die "container $id is not running"
    if [ "${{1:-}}" = "--" ]; then
      echo "Error: failed to find target executable --" 1>&2
      exit 1
    fi
    [ "$#" -gt 0 ] || die "no command"
    "$@"
    ;;

  build) echo "Successfully built fake-image" ;;

  logs)
    echo "line one"
    echo "line two" 1>&2
    ;;

  *) die "unknown subcommand: $cmd" ;;
esac
"#,
        state = state.display()
    );
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&script, perm).unwrap();
    (script, state)
}

fn ubuntu() -> ImageRef {
    ImageRef {
        registry: None,
        repository: "ubuntu".into(),
        tag: Some("24.04".into()),
        digest: None,
    }
}

fn spec(name: &str) -> ContainerSpec {
    ContainerSpec {
        name: name.into(),
        image: ubuntu(),
        // Long-running command, matching what `to_container_spec`
        // injects for Apple containers.
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

// ---------------------------------------------------------------------------
// End-to-end smoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn end_to_end_against_fake_cli() {
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());

    rt.pull(&ubuntu()).await.unwrap();
    let cid = rt.create(&spec("dev")).await.unwrap();
    assert_eq!(cid, "dev");

    // start polls inspect; the fake reports "created" once before
    // promoting to running, so this verifies the poll loop too.
    rt.start(&cid).await.unwrap();

    let status = rt.inspect(&cid).await.unwrap();
    assert_eq!(status.container_id, "dev");
    assert_eq!(status.state, ContainerState::Running);
    assert_eq!(status.image_ref.as_deref(), Some("ubuntu:24.04"));

    let list = rt.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].container_id, "dev");
    assert_eq!(list[0].state, ContainerState::Running);
    assert_eq!(list[0].image_ref.as_deref(), Some("ubuntu:24.04"));

    let mut rx = rt
        .logs(
            &cid,
            &LogOptions {
                follow: false,
                tail: None,
            },
        )
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .ok()
        .flatten()
    {
        got.push(chunk.line);
    }
    got.sort();
    assert_eq!(got, vec!["line one".to_string(), "line two".to_string()]);

    rt.stop(&cid).await.unwrap();
    rt.remove(&cid, true).await.unwrap();
}

// ---------------------------------------------------------------------------
// Regression tests for previously-shipped bugs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inspect_does_not_pass_format_flag() {
    // Real CLI rejects `--format` with exit code 64; if the adapter ever
    // adds it back, the strict fake fails inspect and this test fires.
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    rt.create(&spec("dev")).await.unwrap();
    rt.start("dev").await.unwrap();
    let status = rt
        .inspect("dev")
        .await
        .expect("inspect must not pass --format");
    assert_eq!(status.state, ContainerState::Running);
}

#[tokio::test]
async fn exec_uses_no_dash_dash_separator() {
    // Real CLI treats a literal `--` as the executable name. The fake
    // mirrors that: if the adapter regresses, exec fails with
    // "failed to find target executable --".
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    rt.create(&spec("dev")).await.unwrap();
    rt.start("dev").await.unwrap();
    let result = rt
        .exec("dev", &exec_opts(&["/bin/sh", "-c", "echo hello"]))
        .await
        .expect("exec must not pass `--`");
    assert_eq!(result.exit_code, 0);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("hello"), "stdout={stdout:?}");
}

#[tokio::test]
async fn list_parses_nested_configuration_id() {
    // `container list --format json` emits identity under `configuration`.
    // Without nested-id support, container_id would come back empty and
    // any subsequent stop/remove would target "". The strict fake only
    // emits the nested shape, so this is an end-to-end check.
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    rt.create(&spec("alpha")).await.unwrap();
    rt.create(&spec("beta")).await.unwrap();
    let list = rt.list().await.unwrap();
    let ids: Vec<_> = list.iter().map(|s| s.container_id.clone()).collect();
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert!(ids.contains(&"alpha".to_string()));
    assert!(ids.contains(&"beta".to_string()));
}

#[tokio::test]
async fn create_with_duplicate_name_surfaces_already_exists() {
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    rt.create(&spec("dup")).await.unwrap();
    let err = rt
        .create(&spec("dup"))
        .await
        .expect_err("second create with same name must error");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("already exists"),
        "expected 'already exists' in error, got: {msg}",
    );
}

#[tokio::test]
async fn empty_container_id_is_rejected_before_shelling_out() {
    // Avoids the embarrassing
    //   `container delete --force ` exited with status 1: "container with ID  not found"
    // we used to surface in the UI when an empty cid leaked through.
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    for r in [
        rt.start("").await,
        rt.stop("").await,
        rt.remove("", true).await,
        rt.inspect("").await.map(|_| ()),
    ] {
        let err = r.expect_err("empty container id must be rejected");
        match err {
            ContainerRuntimeError::Backend(msg) => {
                assert!(
                    msg.to_ascii_lowercase().contains("missing container id"),
                    "got: {msg}",
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

#[tokio::test]
async fn start_polls_inspect_until_running() {
    // The fake's first `inspect` after `start` reports "created"; the
    // adapter must poll until "running" or the next call (typically an
    // `exec` running postCreateCommand) races into a stopped container.
    let (script, _state) = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());
    rt.create(&spec("racy")).await.unwrap();
    rt.start("racy").await.unwrap();
    let status = rt.inspect("racy").await.unwrap();
    assert_eq!(status.state, ContainerState::Running);
}

// Real-CLI integration tests live in `tests/apple_containers_real.rs`.
