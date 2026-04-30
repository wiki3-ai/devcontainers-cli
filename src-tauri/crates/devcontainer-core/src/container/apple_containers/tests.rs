//! Unit tests for the Apple Containers backend.
//!
//! The argv shaping and JSON parsers are pure functions, so they can be
//! tested without the real `container` binary on `$PATH`. The streaming
//! `logs` path is exercised via a fake shell script that mimics the
//! relevant subcommand surface.

use std::collections::HashMap;
use std::path::PathBuf;

use super::cli::{
    build_args_with_dns, create_args, exec_args, image_label_from_inspect, image_list_contains,
    image_ref_to_string, logs_args, parse_inspect, parse_list, pull_args, remove_args,
    system_status_is_running,
};
use crate::container::traits::{
    BuildSpec, ContainerSpec, ContainerState, ExecOptions, ImageRef, LogOptions, MountKind,
    MountSpec, PortForward, PortProtocol,
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
        run_args: vec![],
        labels: HashMap::new(),
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
        run_args: vec![],
        labels: HashMap::new(),
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
fn create_args_inserts_run_args_before_image() {
    let spec = ContainerSpec {
        name: "rg".into(),
        image: ubuntu(),
        command: None,
        workdir: None,
        env: HashMap::new(),
        mounts: vec![],
        ports: vec![],
        user: None,
        privileged: false,
        run_args: vec!["--cpus=4".into(), "--memory=8g".into()],
        labels: HashMap::new(),
    };
    let args = create_args(&spec);
    let img_idx = args.iter().position(|a| a == "ubuntu:24.04").unwrap();
    // run_args appear contiguously in order, immediately before the image.
    assert_eq!(args[img_idx - 2], "--cpus=4");
    assert_eq!(args[img_idx - 1], "--memory=8g");
}

#[test]
fn remove_args_force_flag() {
    assert_eq!(remove_args("abc", false), vec!["delete", "abc"]);
    assert_eq!(remove_args("abc", true), vec!["delete", "--force", "abc"]);
}

#[test]
fn build_args_appends_dns_before_context() {
    let spec = BuildSpec {
        tag: ubuntu(),
        context_dir: PathBuf::from("/ctx"),
        dockerfile: PathBuf::from("/ctx/Dockerfile"),
        build_args: HashMap::new(),
        target: None,
        labels: HashMap::new(),
    };
    let dns = vec!["192.168.1.1".to_string(), "1.1.1.1".to_string()];
    let a = build_args_with_dns(&spec, &dns);
    let ctx_idx = a.iter().position(|x| x == "/ctx").unwrap();
    // `--dns IP --dns IP` immediately precedes the context dir.
    assert_eq!(a[ctx_idx - 4], "--dns");
    assert_eq!(a[ctx_idx - 3], "192.168.1.1");
    assert_eq!(a[ctx_idx - 2], "--dns");
    assert_eq!(a[ctx_idx - 1], "1.1.1.1");
}

#[test]
fn build_args_with_no_dns_omits_flag() {
    let spec = BuildSpec {
        tag: ubuntu(),
        context_dir: PathBuf::from("/ctx"),
        dockerfile: PathBuf::from("/ctx/Dockerfile"),
        build_args: HashMap::new(),
        target: None,
        labels: HashMap::new(),
    };
    let a = build_args_with_dns(&spec, &[]);
    assert!(!a.iter().any(|x| x == "--dns"));
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
        ..Default::default()
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

#[test]
fn parse_inspect_extracts_host_mount_sources() {
    // Trimmed copy of real `container inspect <name>` output. The
    // dashboard joins `host_mounts[*]` against known workspace paths to
    // render the repo↔container link without relying on the container
    // having the same name as the repo folder.
    let s = r#"[{
        "configuration":{
            "id":"JupyterLite-Demo",
            "image":{"reference":"devcontainer-take-two:latest"},
            "mounts":[
                {"type":{"virtiofs":{}},"source":"/Users/jim/Wiki3/take-two","options":[],"destination":"/workspaces/take-two"}
            ]
        },
        "status":"running"
    }]"#;
    let st = parse_inspect(s, "JupyterLite-Demo").unwrap();
    assert_eq!(st.container_id, "JupyterLite-Demo");
    assert_eq!(st.state, ContainerState::Running);
    assert_eq!(
        st.host_mounts,
        vec!["/Users/jim/Wiki3/take-two".to_string()]
    );
}

#[test]
fn parse_inspect_no_mounts_yields_empty_vec() {
    let s = r#"{"id":"x","status":"running"}"#;
    let st = parse_inspect(s, "x").unwrap();
    assert!(st.host_mounts.is_empty());
}

#[test]
fn system_status_running_recognised() {
    let stdout = "FIELD              VALUE\nstatus             running\nappRoot            /tmp\n";
    assert!(system_status_is_running(stdout));
}

#[test]
fn system_status_stopped_recognised() {
    let stdout = "FIELD              VALUE\nstatus             stopped\n";
    assert!(!system_status_is_running(stdout));
}

#[test]
fn system_status_empty_treated_as_not_running() {
    assert!(!system_status_is_running(""));
    assert!(!system_status_is_running("garbage output\n"));
}

#[test]
fn image_list_contains_matches_reference() {
    let stdout = r#"[
        {"reference":"alpine:3.19"},
        {"reference":"devcontainer-foo:latest"}
    ]"#;
    assert!(image_list_contains(stdout, "devcontainer-foo:latest"));
    assert!(!image_list_contains(stdout, "devcontainer-bar:latest"));
    assert!(!image_list_contains("", "devcontainer-foo:latest"));
    assert!(!image_list_contains("not json", "devcontainer-foo:latest"));
}

#[test]
fn image_list_contains_matches_implicit_docker_io_prefix() {
    // Apple's `container image list` reports Docker Hub images with the
    // implicit `docker.io/library/` prefix expanded, even when they
    // were pulled with the bare `library/alpine` ref. The cache-hit
    // probe must still fire in that case.
    let stdout = r#"[
        {"reference":"docker.io/library/alpine:3.19"},
        {"reference":"docker.io/library/ubuntu:24.04"}
    ]"#;
    assert!(image_list_contains(stdout, "library/alpine:3.19"));
    assert!(image_list_contains(stdout, "docker.io/library/alpine:3.19"));
    assert!(!image_list_contains(stdout, "library/busybox:latest"));
}

#[test]
fn image_label_from_inspect_reads_nested_labels() {
    let stdout = r#"[{
        "variants":[
            {"platform":{"os":"linux","architecture":"arm64"},
             "config":{"config":{"Labels":{
                "org.devcontainers.config_hash":"abc123",
                "maintainer":"jim"
             }}}}
        ]
    }]"#;
    assert_eq!(
        image_label_from_inspect(stdout, "org.devcontainers.config_hash"),
        Some("abc123".to_string())
    );
    assert_eq!(image_label_from_inspect(stdout, "missing"), None);
    assert_eq!(image_label_from_inspect("", "k"), None);
}

#[test]
fn build_args_emits_sorted_labels() {
    let mut labels = HashMap::new();
    labels.insert("zeta".into(), "Z".into());
    labels.insert("alpha".into(), "A".into());
    let spec = BuildSpec {
        tag: ubuntu(),
        context_dir: PathBuf::from("/ctx"),
        dockerfile: PathBuf::from("/ctx/Dockerfile"),
        build_args: HashMap::new(),
        target: None,
        labels,
    };
    let a = build_args_with_dns(&spec, &[]);
    let alpha = a.iter().position(|x| x == "alpha=A").unwrap();
    let zeta = a.iter().position(|x| x == "zeta=Z").unwrap();
    assert!(alpha < zeta, "labels must be sorted: {a:?}");
    // Each label is preceded by `--label`.
    assert_eq!(a[alpha - 1], "--label");
    assert_eq!(a[zeta - 1], "--label");
}

#[test]
fn create_args_emits_label_flag_before_run_args() {
    let mut labels = HashMap::new();
    labels.insert("org.devcontainers.config_hash".into(), "deadbeef".into());
    let spec = ContainerSpec {
        name: "lbl".into(),
        image: ubuntu(),
        command: None,
        workdir: None,
        env: HashMap::new(),
        mounts: vec![],
        ports: vec![],
        user: None,
        privileged: false,
        run_args: vec!["--cpus=2".into()],
        labels,
    };
    let args = create_args(&spec);
    let label_flag = args.iter().position(|a| a == "--label").unwrap();
    let label_val = label_flag + 1;
    let runarg = args.iter().position(|a| a == "--cpus=2").unwrap();
    assert_eq!(args[label_val], "org.devcontainers.config_hash=deadbeef");
    assert!(label_val < runarg, "labels precede run_args: {args:?}");
}
