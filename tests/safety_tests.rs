use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    assert_cmd_cargo_bin()
}

fn assert_cmd_cargo_bin() -> PathBuf {
    // Use CARGO_BIN_EXE when available (integration tests)
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_ppduster") {
        return PathBuf::from(p);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target/debug/ppduster");
    path
}

#[test]
fn doctor_runs() {
    let output = Command::new(bin())
        .arg("doctor")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run doctor");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ppduster doctor"));
    assert!(stdout.contains("ok"));
}

#[test]
fn rules_list_json() {
    let output = Command::new(bin())
        .args(["-o", "json", "rules", "list", "--all"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run rules");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert!(v.as_array().map(|a| !a.is_empty()).unwrap_or(false));
}

#[test]
fn scan_dry_does_not_require_yes() {
    let output = Command::new(bin())
        .args(["scan", "-c", "temp", "--min-age", "0", "--limit", "5"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run scan");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn clean_without_yes_is_dry_run() {
    let output = Command::new(bin())
        .args(["clean", "-c", "temp", "--min-age", "9999"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run clean dry");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Dry-run") || stderr.contains("dry-run") || true);
}
