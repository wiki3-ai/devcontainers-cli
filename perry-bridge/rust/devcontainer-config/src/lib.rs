//! `devcontainer-config` — embed-friendly Rust client for the perry-bridge
//! native binary.
//!
//! See `perry-bridge/PROTOCOL.md` for the wire contract. The public API is
//! deliberately small:
//!
//! * [`load_devcontainer_config`] — convenience: extract + spawn the embedded
//!   binary (Tauri "embed" mode). Requires the `embed` cargo feature, which
//!   is on by default.
//! * [`load_devcontainer_config_with_binary`] — pass an explicit path to a
//!   binary. Use this in Tauri sidecar mode (see `TAURI_INTEGRATION.md`).
//!
//! Both return `serde_json::Value` matching the JSON projection of
//! `DevContainerConfig` from `src/spec-configuration/configuration.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};

const SUPPORTED_PROTOCOL: u32 = 1;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("perry binary not found at {0}; run scripts/build-perry.sh")]
    BinaryNotFound(PathBuf),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("tool reported error: {message}")]
    Tool {
        message: String,
        stack: Option<String>,
    },

    #[error("host call failed ({code:?}): {message}")]
    Host {
        code: Option<String>,
        message: String,
    },

    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Options for `load_devcontainer_config*`.
#[derive(Debug, Default, Clone)]
pub struct LoadOptions {
    /// Optional explicit path to the devcontainer.json. When `None`, the
    /// binary searches the well-known paths under the workspace folder.
    pub config_file: Option<PathBuf>,
    /// Environment to expose to variable substitution. Defaults to the
    /// current process environment.
    pub env: Option<HashMap<String, String>>,
    /// Override the platform reported to the binary. Defaults to the host.
    pub platform: Option<String>,
}

/// Load and parse a devcontainer.json using the embedded Perry binary.
///
/// Requires the `embed` feature. On first call, extracts the embedded binary
/// to a content-addressed file under `std::env::temp_dir()`, marks it
/// executable on Unix, and spawns it.
#[cfg(feature = "embed")]
pub async fn load_devcontainer_config(workspace: &Path) -> Result<Value, BridgeError> {
    let bin = embedded::extract_binary().await?;
    load_devcontainer_config_with_binary(&bin, workspace, None).await
}

/// Load and parse a devcontainer.json by spawning the binary at `bin_path`.
///
/// Use this from Tauri sidecar setups: pass `app.shell().sidecar(...)`'s
/// resolved path here.
pub async fn load_devcontainer_config_with_binary(
    bin_path: &Path,
    workspace: &Path,
    options: Option<LoadOptions>,
) -> Result<Value, BridgeError> {
    if !bin_path.exists() {
        return Err(BridgeError::BinaryNotFound(bin_path.to_path_buf()));
    }
    let opts = options.unwrap_or_default();

    let env = opts.env.unwrap_or_else(|| std::env::vars().collect());
    let platform = opts.platform.unwrap_or_else(default_platform);

    let mut child = Command::new(bin_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| BridgeError::Protocol("child has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BridgeError::Protocol("child has no stdout".into()))?;

    let mut session = Session {
        stdin,
        stdout: BufReader::new(stdout).lines(),
    };

    // Read hello envelope.
    let hello_line = session
        .next_line()
        .await?
        .ok_or_else(|| BridgeError::Protocol("child closed before hello".into()))?;
    let hello: HelloEnvelope = serde_json::from_str(&hello_line)?;
    if hello.kind != "hello" {
        return Err(BridgeError::Protocol(format!(
            "expected hello, got kind={}",
            hello.kind
        )));
    }
    if hello.protocol != SUPPORTED_PROTOCOL {
        return Err(BridgeError::Protocol(format!(
            "unsupported protocol version {}; this crate supports {}",
            hello.protocol, SUPPORTED_PROTOCOL
        )));
    }

    // Send the request.
    let mut config_file_str: Option<String> = None;
    if let Some(p) = opts.config_file.as_ref() {
        config_file_str = Some(p.to_string_lossy().into_owned());
    }

    let request = json!({
        "kind": "request",
        "command": "loadConfig",
        "workspaceFolder": workspace.to_string_lossy(),
        "configFile": config_file_str,
        "platform": platform,
        "env": env,
    });
    session.send(&request).await?;

    // Service host calls until we get a result or error.
    let result = loop {
        let line = session
            .next_line()
            .await?
            .ok_or_else(|| BridgeError::Protocol("child closed before result".into()))?;
        let msg: Value = serde_json::from_str(&line)?;
        match msg.get("kind").and_then(Value::as_str) {
            Some("host") => {
                let id = msg.get("id").and_then(Value::as_u64).ok_or_else(|| {
                    BridgeError::Protocol("host call missing id".into())
                })?;
                let op = msg
                    .get("op")
                    .and_then(Value::as_str)
                    .ok_or_else(|| BridgeError::Protocol("host call missing op".into()))?;
                let args = msg.get("args").cloned().unwrap_or(Value::Null);
                let reply = handle_host_call(op, &args).await;
                let envelope = match reply {
                    Ok(value) => json!({"kind":"host-reply","id":id,"ok":true,"value":value}),
                    Err((code, message)) => json!({
                        "kind":"host-reply","id":id,"ok":false,
                        "error":{"code":code,"message":message},
                    }),
                };
                session.send(&envelope).await?;
            }
            Some("result") => {
                break msg.get("value").cloned().unwrap_or(Value::Null);
            }
            Some("error") => {
                let message = msg
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)")
                    .to_string();
                let stack = msg
                    .get("stack")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                let _ = child.wait().await;
                return Err(BridgeError::Tool { message, stack });
            }
            other => {
                return Err(BridgeError::Protocol(format!(
                    "unexpected envelope kind: {other:?}"
                )));
            }
        }
    };

    let status = child.wait().await?;
    if !status.success() {
        return Err(BridgeError::Protocol(format!(
            "child exited with status {status} after emitting result"
        )));
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HelloEnvelope {
    kind: String,
    protocol: u32,
    #[serde(default)]
    #[allow(dead_code)]
    slice: Option<String>,
}

struct Session {
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl Session {
    async fn send(&mut self, value: &Value) -> Result<(), BridgeError> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn next_line(&mut self) -> Result<Option<String>, BridgeError> {
        Ok(self.stdout.next_line().await?)
    }
}

async fn handle_host_call(op: &str, args: &Value) -> Result<Value, (Option<String>, String)> {
    match op {
        "fs.readFile" => {
            let p = path_arg(args)?;
            match tokio::fs::read(&p).await {
                Ok(bytes) => Ok(json!({
                    "bytesBase64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                })),
                Err(e) => Err((io_code(&e), e.to_string())),
            }
        }
        "fs.writeFile" => {
            let p = path_arg(args)?;
            let b64 = args
                .get("bytesBase64")
                .and_then(Value::as_str)
                .ok_or((None, "missing bytesBase64".into()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| (None, format!("base64: {e}")))?;
            tokio::fs::write(&p, bytes)
                .await
                .map_err(|e| (io_code(&e), e.to_string()))?;
            Ok(json!({}))
        }
        "fs.stat" => {
            let p = path_arg(args)?;
            match tokio::fs::metadata(&p).await {
                Ok(md) => {
                    let kind = if md.is_file() {
                        "file"
                    } else if md.is_dir() {
                        "dir"
                    } else {
                        "other"
                    };
                    Ok(json!({"kind": kind, "size": md.len()}))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(json!({"kind": "missing"}))
                }
                Err(e) => Err((io_code(&e), e.to_string())),
            }
        }
        "fs.readDir" => {
            let p = path_arg(args)?;
            let mut rd = tokio::fs::read_dir(&p)
                .await
                .map_err(|e| (io_code(&e), e.to_string()))?;
            let mut entries = Vec::new();
            while let Some(entry) = rd
                .next_entry()
                .await
                .map_err(|e| (io_code(&e), e.to_string()))?
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                let ft = entry
                    .file_type()
                    .await
                    .map_err(|e| (io_code(&e), e.to_string()))?;
                let kind = if ft.is_file() {
                    "file"
                } else if ft.is_dir() {
                    "dir"
                } else {
                    "other"
                };
                entries.push(json!({"name": name, "kind": kind}));
            }
            Ok(json!({"entries": entries}))
        }
        other => Err((None, format!("unsupported op: {other}"))),
    }
}

fn path_arg(args: &Value) -> Result<PathBuf, (Option<String>, String)> {
    args.get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or((None, "missing path".into()))
}

fn io_code(e: &std::io::Error) -> Option<String> {
    use std::io::ErrorKind::*;
    Some(
        match e.kind() {
            NotFound => "ENOENT",
            PermissionDenied => "EACCES",
            AlreadyExists => "EEXIST",
            InvalidInput => "EINVAL",
            _ => return e.raw_os_error().map(|c| format!("E{c}")),
        }
        .to_string(),
    )
}

fn default_platform() -> String {
    if cfg!(target_os = "windows") {
        "win32".into()
    } else if cfg!(target_os = "macos") {
        "darwin".into()
    } else {
        "linux".into()
    }
}

// ---------------------------------------------------------------------------
// Embedded binary support (feature = "embed")
// ---------------------------------------------------------------------------

#[cfg(feature = "embed")]
mod embedded {
    use super::BridgeError;
    use std::path::PathBuf;
    use tokio::io::AsyncWriteExt;

    // The build script writes the path of the expected binary into
    // PERRY_BIN_PATH. We probe at runtime instead of include_bytes!-ing,
    // because include_bytes! requires the file to exist at compile time and
    // we want a graceful runtime error if a developer forgot to run
    // scripts/build-perry.sh.
    pub async fn extract_binary() -> Result<PathBuf, BridgeError> {
        let src_path = PathBuf::from(env!("PERRY_BIN_PATH"));
        if !src_path.exists() {
            return Err(BridgeError::BinaryNotFound(src_path));
        }

        // Copy to a per-version temp file so concurrent processes don't
        // clobber each other. We use a content hash via the file size +
        // mtime as a cheap key; if you need stronger guarantees, switch to
        // a real digest.
        let md = tokio::fs::metadata(&src_path).await?;
        let key = format!(
            "perry-{}-{}",
            md.len(),
            md.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let filename = env!("PERRY_BIN_FILENAME");
        let dst_dir = std::env::temp_dir().join(key);
        let dst_path = dst_dir.join(filename);
        if dst_path.exists() {
            return Ok(dst_path);
        }

        tokio::fs::create_dir_all(&dst_dir).await?;
        let bytes = tokio::fs::read(&src_path).await?;
        let mut f = tokio::fs::File::create(&dst_path).await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        drop(f);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&dst_path, perms)?;
        }

        Ok(dst_path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_platform_is_one_of_three() {
        let p = default_platform();
        assert!(p == "linux" || p == "darwin" || p == "win32", "got {p}");
    }

    #[tokio::test]
    async fn read_file_and_stat_round_trip() {
        // Direct exercise of the host-call handlers with no Perry binary;
        // verifies the Rust side of the protocol against the local FS.
        let dir = tempdir();
        let path = dir.join("hello.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();

        let stat = handle_host_call("fs.stat", &json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(stat["kind"], "file");
        assert_eq!(stat["size"], 2);

        let read = handle_host_call("fs.readFile", &json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        let b64 = read["bytesBase64"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(bytes, b"hi");
    }

    #[tokio::test]
    async fn stat_missing_returns_kind_missing() {
        let dir = tempdir();
        let r = handle_host_call(
            "fs.stat",
            &json!({"path": dir.join("nope").to_string_lossy()}),
        )
        .await
        .unwrap();
        assert_eq!(r["kind"], "missing");
    }

    #[tokio::test]
    async fn read_dir_lists_entries() {
        let dir = tempdir();
        tokio::fs::write(dir.join("a.txt"), b"x").await.unwrap();
        tokio::fs::create_dir(dir.join("sub")).await.unwrap();

        let r = handle_host_call("fs.readDir", &json!({"path": dir.to_string_lossy()}))
            .await
            .unwrap();
        let entries = r["entries"].as_array().unwrap();
        let mut names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);
    }

    #[tokio::test]
    async fn binary_not_found_when_missing() {
        let bogus = PathBuf::from("/definitely/does/not/exist/perry-bridge-bin");
        let err = load_devcontainer_config_with_binary(&bogus, &PathBuf::from("/tmp"), None)
            .await
            .unwrap_err();
        assert!(matches!(err, BridgeError::BinaryNotFound(_)));
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "devcontainer-config-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}
