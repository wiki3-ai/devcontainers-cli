// End-to-end test that does NOT require Perry. We rely on a small companion
// `[[bin]] fake-perry` (see tests/support/fake_perry.rs) that follows the
// same NDJSON protocol as PROTOCOL.md and is built only when the
// `test-fakes` feature is enabled. To run:
//
//     cargo test --features test-fakes -- --include-ignored

use std::path::PathBuf;

#[tokio::test]
#[cfg(feature = "test-fakes")]
async fn end_to_end_with_fake_binary() {
    // Locate the freshly-built fake-perry bin produced by cargo. When this
    // test runs, cargo has already built it under target/<profile>/.
    let bin = locate_fake_perry().expect("fake-perry not built; pass --features test-fakes");

    let dir = tempdir();
    let workspace = dir.join("ws");
    std::fs::create_dir_all(workspace.join(".devcontainer")).unwrap();
    std::fs::write(
        workspace.join(".devcontainer/devcontainer.json"),
        r#"{"image":"ubuntu:latest","name":"e2e"}"#,
    )
    .unwrap();

    let value =
        devcontainer_config::load_devcontainer_config_with_binary(&bin, &workspace, None)
            .await
            .unwrap();
    assert_eq!(value["config"]["image"], "ubuntu:latest");
    assert_eq!(value["config"]["name"], "e2e");
}

#[cfg(feature = "test-fakes")]
fn locate_fake_perry() -> Option<PathBuf> {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests when the
    // bin target exists in the same package.
    option_env!("CARGO_BIN_EXE_fake-perry").map(PathBuf::from)
}

fn tempdir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let p = std::env::temp_dir().join(format!(
        "perry-bridge-e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}
