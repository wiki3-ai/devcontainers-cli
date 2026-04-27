//! Integration tests gated on the `apple-containers-live` feature.
//!
//! These tests do *not* require macOS or the real `container` CLI; they
//! drive [`AppleContainersRuntime`] with a small fake binary placed in a
//! tempdir. The intent is to lock the wire-level shape of our `container …`
//! invocations and exercise the streaming `logs` path end-to-end.

#![cfg(feature = "apple-containers-live")]

use std::time::Duration;

use devcontainers_app_lib::container::{
    apple_containers::AppleContainersRuntime, ContainerRuntime, ContainerSpec, ImageRef, LogOptions,
};

fn fake_container_script() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "devcontainers-fake-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("container");
    let body = r#"#!/usr/bin/env bash
set -e
case "$1" in
  --version) echo "container 0.0.0-fake" ;;
  image)
    if [ "$2" = "pull" ]; then echo "pulled $3"; fi
    ;;
  create) echo "fake-cid-1" ;;
  start|stop) ;;
  delete) ;;
  inspect)
    cid="$2"
    echo "{\"id\":\"$cid\",\"status\":\"running\",\"image\":{\"reference\":\"ubuntu:24.04\"}}"
    ;;
  list) echo '[{"id":"fake-cid-1","status":"running"}]' ;;
  exec)
    shift
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
    if [ "$1" = "--" ]; then shift; fi
    "$@"
    ;;
  logs)
    echo "line one"
    echo "line two" 1>&2
    if [ "$2" = "--follow" ]; then sleep 0.05; fi
    ;;
esac
"#;
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&script, perm).unwrap();
    script
}

#[tokio::test]
async fn end_to_end_against_fake_cli() {
    let script = fake_container_script();
    let rt = AppleContainersRuntime::with_binary(script.display().to_string());

    let img = ImageRef {
        registry: None,
        repository: "ubuntu".into(),
        tag: Some("24.04".into()),
        digest: None,
    };
    rt.pull(&img).await.unwrap();

    let spec = ContainerSpec {
        name: "dev".into(),
        image: img,
        command: None,
        workdir: None,
        env: Default::default(),
        mounts: vec![],
        ports: vec![],
        user: None,
        privileged: false,
    };
    let cid = rt.create(&spec).await.unwrap();
    assert_eq!(cid, "fake-cid-1");

    rt.start(&cid).await.unwrap();
    let status = rt.inspect(&cid).await.unwrap();
    assert_eq!(status.container_id, "fake-cid-1");

    let list = rt.list().await.unwrap();
    assert_eq!(list.len(), 1);

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
