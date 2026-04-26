//! Thin CLI wrapper around the library — exists mainly so that the same code
//! path the Tauri app uses can be exercised from a shell during development.
//!
//! Usage:
//!     devcontainer-config-cli <workspace-folder>
//!     devcontainer-config-cli --bin <path-to-perry-binary> <workspace-folder>

use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bin: Option<PathBuf> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--bin" => bin = iter.next().map(PathBuf::from),
            "-h" | "--help" => {
                eprintln!("usage: devcontainer-config-cli [--bin <perry-binary>] <workspace>");
                return ExitCode::from(0);
            }
            other => workspace = Some(PathBuf::from(other)),
        }
    }
    let workspace = match workspace {
        Some(w) => w,
        None => {
            eprintln!("error: workspace folder is required");
            return ExitCode::from(2);
        }
    };

    let result = match bin {
        Some(p) => {
            devcontainer_config::load_devcontainer_config_with_binary(&p, &workspace, None).await
        }
        None => {
            #[cfg(feature = "embed")]
            {
                devcontainer_config::load_devcontainer_config(&workspace).await
            }
            #[cfg(not(feature = "embed"))]
            {
                eprintln!("error: built without `embed` feature; pass --bin <path>");
                return ExitCode::from(2);
            }
        }
    };

    match result {
        Ok(value) => {
            match serde_json::to_string_pretty(&value) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("error: serialise: {e}");
                    return ExitCode::from(1);
                }
            }
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
