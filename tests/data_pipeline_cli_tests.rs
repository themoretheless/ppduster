use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn stdin_transformation_preserves_integer_precision_and_writes_response() {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("response.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .args([
            "--audit-log",
            dir.path().join("audit.jsonl").to_str().unwrap(),
            "data",
            "--response",
            response.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"action":"transform","json":"[{\"id\":9007199254740993},{\"id\":9007199254740993}]","path":"","fields":["id"],"delimiter":"\n","skip_empty":true,"unique":true}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(response).unwrap()).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["output"], "9007199254740993");
}

#[test]
fn invalid_request_returns_nonzero_and_machine_readable_error() {
    let dir = tempfile::tempdir().unwrap();
    let request = dir.path().join("request.json");
    std::fs::write(&request, r#"{"action":"analyze","json":"{"}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .arg("--audit-log")
        .arg(dir.path().join("audit.jsonl"))
        .args(["data", "--request"])
        .arg(request)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["error"]["message"].is_string());
}
