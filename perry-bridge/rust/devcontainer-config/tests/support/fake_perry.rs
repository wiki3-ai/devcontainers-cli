//! Fake "Perry binary" that follows the PROTOCOL.md NDJSON contract well
//! enough to exercise `load_devcontainer_config_with_binary` end-to-end
//! without an actual Perry build. Used only by tests/e2e.rs.

use std::io::{BufRead, BufReader, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdin = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();

    writeln!(
        stdout,
        r#"{{"kind":"hello","protocol":1,"slice":"fake@0.0.0"}}"#
    )
    .unwrap();
    stdout.flush().unwrap();

    let mut line = String::new();
    stdin.read_line(&mut line).unwrap();
    let req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let workspace = req["workspaceFolder"].as_str().unwrap().to_string();
    let path = format!("{workspace}/.devcontainer/devcontainer.json");

    // host call: stat
    writeln!(
        stdout,
        "{}",
        serde_json::json!({"kind":"host","id":1,"op":"fs.stat","args":{"path": &path}})
    )
    .unwrap();
    stdout.flush().unwrap();
    let mut reply = String::new();
    stdin.read_line(&mut reply).unwrap();
    let stat: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
    assert_eq!(stat["ok"], true, "stat must succeed: {reply}");
    assert_eq!(stat["value"]["kind"], "file");

    // host call: readFile
    reply.clear();
    writeln!(
        stdout,
        "{}",
        serde_json::json!({"kind":"host","id":2,"op":"fs.readFile","args":{"path": &path}})
    )
    .unwrap();
    stdout.flush().unwrap();
    stdin.read_line(&mut reply).unwrap();
    let read: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
    assert_eq!(read["ok"], true);
    let b64 = read["value"]["bytesBase64"].as_str().unwrap();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    let content = String::from_utf8(bytes).unwrap();

    // Strict JSON only — the test fixture has no comments. The real Perry
    // binary uses jsonc-parser; this fake is just enough to prove the wire.
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    let result = serde_json::json!({
        "kind": "result",
        "value": {
            "config": parsed.clone(),
            "raw": parsed,
            "configFilePath": path,
        }
    });
    writeln!(stdout, "{result}").unwrap();
    stdout.flush().unwrap();
}
