//! Cargo-runnable example: parse `example-usage/workspace/.devcontainer/devcontainer.json`
//! using the perry-bridge crate, with no Node / Deno / Bun installed.
//!
//! See `README.md` in this directory for what this example replaces and what
//! it does *not* replace.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .ok_or("no parent of CARGO_MANIFEST_DIR")?
        .join("workspace");

    let value = devcontainer_config::load_devcontainer_config(&workspace).await?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
