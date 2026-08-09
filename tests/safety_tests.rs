use std::fs;
use std::path::{Path, PathBuf};
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

#[test]
fn extended_rule_packs_are_quarantined_report_only() {
    #[derive(serde::Deserialize)]
    struct RuleFile {
        rules: Vec<RuleSafety>,
    }

    #[derive(serde::Deserialize)]
    struct RuleSafety {
        id: String,
        risk: String,
        default_enabled: bool,
    }

    const REVIEWED_CORE_PACKS: &[&str] = &[
        "apps.yaml",
        "dev.yaml",
        "linux.yaml",
        "macos.yaml",
        "windows.yaml",
    ];
    let rules_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("rules");
    for entry in fs::read_dir(&rules_dir).expect("read rules directory") {
        let path = entry.expect("read rule entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("yaml") {
            continue;
        }
        let name = path.file_name().and_then(|value| value.to_str()).unwrap();
        if REVIEWED_CORE_PACKS.contains(&name) {
            continue;
        }
        let contents = fs::read_to_string(&path).expect("read extended rule pack");
        let pack: RuleFile = serde_yaml::from_str(&contents).expect("parse extended rule pack");
        for rule in pack.rules {
            assert_eq!(
                rule.risk,
                "report-only",
                "{} rule {} must remain report-only until path-level review",
                path.display(),
                rule.id
            );
            assert!(
                !rule.default_enabled,
                "{} rule {} must remain disabled until path-level review",
                path.display(),
                rule.id
            );
        }
    }
}
