//! Unit tests for the Apple Containers backend.
//!
//! The argv shaping and JSON parsers are pure functions, so they can be
//! tested without the real `container` binary on `$PATH`. The streaming
//! `logs` path is exercised via a fake shell script that mimics the
//! relevant subcommand surface.

use std::collections::HashMap;
use std::path::PathBuf;

use super::cli::{
    create_args, exec_args, image_ref_to_string, logs_args, parse_inspect, parse_list, pull_args,
    remove_args,
};
use crate::container::traits::{
    ContainerSpec, ContainerState, ExecOptions, ImageRef, LogOptions, MountKind, MountSpec,
    PortForward, PortProtocol,
};

fn ubuntu() -> ImageRef {
    ImageRef {
        registry: None,
        repository: "ubuntu".into(),
        tag: Some("24.04".into()),
        digest: None,
    }
}

#[test]
fn image_ref_roundtrip() {
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
fn pull_args_minimal() {
    assert_eq!(
        pull_args(&ubuntu()),
        vec!["image".to_string(), "pull".into(), "ubuntu:24.04".into()]
    );
}

#[test]
fn create_args_sorts_env_and_appends_image_last() {
    let mut env = HashMap::new();
    env.insert("Z".to_string(), "z".to_string());
    env.insert("A".to_string(), "a".to_string());
    let spec = ContainerSpec {
        name: "dev".into(),
        image: ubuntu(),
        command: Some(vec!["bash".into(), "-l".into()]),
        workdir: Some(PathBuf::from("/work")),
        env,
        mounts: vec![MountSpec {
            kind: MountKind::Bind,
            source: PathBuf::from("/host/src"),
            target: PathBuf::from("/work"),
            read_only: false,
        }],
        ports: vec![PortForward {
            host_port: 3000,
            container_port: 3000,
            protocol: PortProtocol::Tcp,
        }],
        user: Some("vscode".into()),
        privileged: false,
    };
    let args = create_args(&spec);

    // First three args are deterministic.
    assert_eq!(&args[0..3], &["create", "--name", "dev"]);

    // The image must come immediately before the command.
    let img_idx = args.iter().position(|a| a == "ubuntu:24.04").unwrap();
    assert_eq!(args[img_idx + 1], "bash");
    assert_eq!(args[img_idx + 2], "-l");
    assert_eq!(img_idx + 3, args.len());

    // Env pairs are sorted by value (which mirrors key in this case).
    let env_a = args.iter().position(|a| a == "A=a").unwrap();
    let env_z = args.iter().position(|a| a == "Z=z").unwrap();
    assert!(env_a < env_z, "env should be sorted: {args:?}");

    // Mount flag is the well-known `type=bind,...` shape.
    let mount = args.iter().position(|a| a == "--mount").unwrap();
    assert_eq!(args[mount + 1], "type=bind,source=/host/src,target=/work");

    // Port publish uses host:container/proto.
    let publish = args.iter().position(|a| a == "--publish").unwrap();
    assert_eq!(args[publish + 1], "3000:3000/tcp");
}

#[test]
fn create_args_marks_readonly_bind() {
    let spec = ContainerSpec {
        name: "ro".into(),
        image: ubuntu(),
        command: None,
        workdir: None,
        env: HashMap::new(),
        mounts: vec![MountSpec {
            kind: MountKind::Bind,
            source: PathBuf::from("/host"),
            target: PathBuf::from("/in"),
            read_only: true,
        }],
        ports: vec![],
        user: None,
        privileged: true,
    };
    let args = create_args(&spec);
    let mount = args.iter().position(|a| a == "--mount").unwrap();
    assert_eq!(
        args[mount + 1],
        "type=bind,source=/host,target=/in,readonly"
    );
    assert!(args.iter().any(|a| a == "--privileged"));
}

#[test]
fn remove_args_force_flag() {
    assert_eq!(remove_args("abc", false), vec!["delete", "abc"]);
    assert_eq!(remove_args("abc", true), vec!["delete", "--force", "abc"]);
}

#[test]
fn exec_args_orders_options() {
    let mut env = HashMap::new();
    env.insert("B".into(), "2".into());
    env.insert("A".into(), "1".into());
    let opts = ExecOptions {
        command: vec!["echo".into(), "hi".into()],
        workdir: Some(PathBuf::from("/w")),
        env,
        user: Some("root".into()),
        tty: true,
    };
    let a = exec_args("cid", &opts);
    assert_eq!(a[0], "exec");
    assert!(a.contains(&"--tty".to_string()));
    let cid = a.iter().position(|s| s == "cid").unwrap();
    // Apple's `container exec` takes the process argv positionally with
    // no `--` separator; the executable name follows the container id.
    assert_eq!(a[cid + 1], "echo");
    assert_eq!(a[cid + 2], "hi");
    assert!(!a.iter().any(|s| s == "--"));
    // env order: A=1 must precede B=2
    let a1 = a.iter().position(|s| s == "A=1").unwrap();
    let b2 = a.iter().position(|s| s == "B=2").unwrap();
    assert!(a1 < b2);
}

#[test]
fn logs_args_follow_and_tail() {
    let opts = LogOptions {
        follow: true,
        tail: Some(50),
    };
    assert_eq!(
        logs_args("cid", &opts),
        vec!["logs", "--follow", "--tail", "50", "cid"]
    );
    let opts = LogOptions::default();
    assert_eq!(logs_args("cid", &opts), vec!["logs", "cid"]);
}

#[test]
fn parse_inspect_object_and_array() {
    let one = r#"{"id":"abc","status":"running","image":{"reference":"ubuntu:24.04"}}"#;
    let s = parse_inspect(one, "abc").unwrap();
    assert_eq!(s.container_id, "abc");
    assert_eq!(s.state, ContainerState::Running);
    assert_eq!(s.image_ref.as_deref(), Some("ubuntu:24.04"));

    let many = r#"[{"id":"x","status":"stopped"}]"#;
    let s = parse_inspect(many, "x").unwrap();
    assert_eq!(s.container_id, "x");
    assert_eq!(s.state, ContainerState::Stopped);
}

#[test]
fn parse_inspect_unknown_state() {
    let s = parse_inspect(r#"{"id":"y"}"#, "y").unwrap();
    assert_eq!(s.state, ContainerState::Unknown);
}

#[test]
fn parse_inspect_rejects_empty() {
    assert!(parse_inspect("", "anything").is_err());
    // Empty array means "not found" — surfaced with that exact phrase
    // so `is_not_found` in the orchestrator recognises it.
    let err = parse_inspect("[]", "ghost").unwrap_err();
    let msg = format!("{err}").to_ascii_lowercase();
    assert!(msg.contains("not found"), "got: {msg}");
    assert!(msg.contains("ghost"), "got: {msg}");
}

#[test]
fn parse_list_empty_and_populated() {
    assert_eq!(parse_list("").unwrap().len(), 0);
    assert_eq!(parse_list("[]").unwrap().len(), 0);
    let v = parse_list(r#"[{"id":"a","status":"running"},{"id":"b","status":"exited"}]"#).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].state, ContainerState::Running);
    assert_eq!(v[1].state, ContainerState::Exited);
}

#[test]
fn parse_list_reads_nested_configuration_id_and_image() {
    // Real shape emitted by `container list --all --format json` (Apple
    // container CLI 0.x): top-level `status`, identity nested under
    // `configuration`. Without nested-id support we'd return an empty
    // container_id and any subsequent stop/remove would fail with
    // "container with ID  not found".
    let s = r#"[{
        "status":"running",
        "configuration":{
            "id":"buildkit",
            "image":{"reference":"ghcr.io/apple/container-builder-shim/builder:0.11.0"}
        }
    }]"#;
    let v = parse_list(s).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].container_id, "buildkit");
    assert_eq!(v[0].state, ContainerState::Running);
    assert_eq!(
        v[0].image_ref.as_deref(),
        Some("ghcr.io/apple/container-builder-shim/builder:0.11.0")
    );
}
