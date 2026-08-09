use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_ppduster")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ppduster"))
}

#[test]
fn app_store_help_is_available_without_a_rule_pack() {
    let workspace = tempfile::tempdir().expect("temporary working directory");
    let output = Command::new(bin())
        .args(["app-store", "--help"])
        .current_dir(workspace.path())
        .output()
        .expect("run app-store help");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for command in ["search", "list", "outdated", "install", "upgrade", "doctor"] {
        assert!(stdout.contains(command), "missing {command} in {stdout}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn app_store_list_json_does_not_load_cleanup_rules() {
    let workspace = tempfile::tempdir().expect("temporary working directory");
    let audit_log = workspace.path().join("audit.jsonl");
    let output = Command::new(bin())
        .arg("--audit-log")
        .arg(audit_log)
        .args(["--output", "json", "app-store", "list"])
        .current_dir(workspace.path())
        .output()
        .expect("run app-store list");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON report");
    assert!(report["scanned_roots"].is_array());
    assert!(report["apps"].is_array());
    assert!(report["warnings"].is_array());
}

#[cfg(target_os = "macos")]
#[test]
fn invalid_search_limit_fails_before_catalog_access() {
    let workspace = tempfile::tempdir().expect("temporary working directory");
    let audit_log = workspace.path().join("audit.jsonl");
    let output = Command::new(bin())
        .arg("--audit-log")
        .arg(audit_log)
        .args([
            "app-store",
            "search",
            "Xcode",
            "--country",
            "US",
            "--limit",
            "0",
        ])
        .current_dir(workspace.path())
        .output()
        .expect("run invalid search");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("search limit must be between 1 and 200"));
}
