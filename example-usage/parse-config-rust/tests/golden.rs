// Acceptance test for the parse-config-rust example.
//
// This is the cheapest test we can write that's still meaningful: confirm
// that the example main returns the expected `BinaryNotFound` error when
// no Perry binary has been built. CI on machines without Perry should run
// this and pass; CI on machines with Perry available should additionally
// run `cargo run` and diff against a golden file (left as a follow-up; see
// .github/workflows/perry.yml).

use std::path::PathBuf;

#[tokio::test]
async fn binary_absent_returns_clear_error_or_succeeds() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.parent().unwrap().join("workspace");

    match devcontainer_config::load_devcontainer_config(&workspace).await {
        Ok(value) => {
            // Perry binary is present and produced a result. Sanity-check
            // the shape so a regression in the slice surfaces here.
            let config = &value["config"];
            assert!(
                config.get("name").is_some()
                    || config.get("image").is_some()
                    || config.get("build").is_some()
                    || config.get("dockerComposeFile").is_some(),
                "expected at least one of name/image/build/dockerComposeFile in {config}"
            );
        }
        Err(devcontainer_config::BridgeError::BinaryNotFound(_)) => {
            // Perry binary not built; acceptable. The error type is what we
            // contract to surface so the Tauri host can present a useful
            // message during development.
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}
