// build.rs
//
// Locates the Perry-compiled binary for the host triple and tells Cargo to
// rerun the build script when it changes. The binary is *not* embedded here
// — `include_bytes!` happens in src/lib.rs gated behind the `embed` feature
// so that crates which only want the sidecar shape (see TAURI_INTEGRATION.md)
// don't need a binary at build time at all.
//
// What we do here:
//   * Compute the expected binary filename for the current host triple.
//   * Export it to the compiler as `PERRY_BIN_FILENAME` env var.
//   * Tell Cargo to invalidate the build if the file appears or changes.

use std::env;
use std::path::PathBuf;

fn main() {
    let triple = env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let ext = if triple.contains("windows") { ".exe" } else { "" };
    let filename = format!("devcontainer-config-{triple}{ext}");

    let bin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin");
    let bin_path = bin_dir.join(&filename);

    // Always export the filename so lib.rs can name the temp-extracted file.
    println!("cargo:rustc-env=PERRY_BIN_FILENAME={filename}");
    println!("cargo:rustc-env=PERRY_BIN_PATH={}", bin_path.display());

    // Re-run if the binary changes (or appears for the first time).
    println!("cargo:rerun-if-changed={}", bin_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    // We do NOT fail the build if the binary is missing. Consumers that want
    // a hard failure can set PERRY_REQUIRE_BIN=1 in their environment.
    if env::var("PERRY_REQUIRE_BIN").as_deref() == Ok("1") && !bin_path.exists() {
        panic!(
            "PERRY_REQUIRE_BIN=1 but {} does not exist. Run scripts/build-perry.sh first.",
            bin_path.display()
        );
    }

    if !bin_path.exists() {
        println!(
            "cargo:warning=perry binary not found at {}. The crate will compile but \
             load_devcontainer_config will return BridgeError::BinaryNotFound at runtime. \
             Run scripts/build-perry.sh to produce it.",
            bin_path.display()
        );
    }
}
