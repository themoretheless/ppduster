use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_ppduster") {
        return PathBuf::from(p);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target/debug/ppduster");
    path
}

fn manifest_dir() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

// ── setup list ────────────────────────────────────────────────────────────────

#[test]
fn setup_list_exits_zero() {
    let output = Command::new(bin())
        .args(["setup", "list"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup list");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn setup_list_shows_example_pack() {
    let output = Command::new(bin())
        .args(["setup", "list"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup list");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("example"),
        "expected 'example' in setup list output, got: {stdout}"
    );
}

#[test]
fn automate_alias_works() {
    // `automate` must still work as an alias for `setup`
    let output = Command::new(bin())
        .args(["automate", "list"])
        .current_dir(manifest_dir())
        .output()
        .expect("run automate list (alias)");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── setup show ────────────────────────────────────────────────────────────────

#[test]
fn setup_show_example_exits_zero() {
    let output = Command::new(bin())
        .args(["setup", "show", "example"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup show example");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pack:"), "expected 'Pack:' in output");
    assert!(stdout.contains("Steps:"), "expected 'Steps:' in output");
}

#[test]
fn setup_show_nonexistent_fails() {
    let output = Command::new(bin())
        .args(["setup", "show", "nonexistent-task-xyz"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup show nonexistent");
    assert!(
        !output.status.success(),
        "expected non-zero exit for unknown pack"
    );
}

#[test]
fn setup_show_warns_on_privileged_steps() {
    let output = Command::new(bin())
        .args(["setup", "show", "example"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup show example");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Example pack has install-dmg and install-pkg — should be flagged
    assert!(
        stdout.contains("install-dmg") || stdout.contains("install-pkg"),
        "expected privileged steps in output: {stdout}"
    );
}

// ── setup run (dry-run) ───────────────────────────────────────────────────────

#[test]
fn setup_run_without_yes_is_safe() {
    // Without --yes the privileged gate fires (example has install-dmg/pkg).
    // Either the gate fires (nonzero) or dry-run prints (zero + "dry-run").
    // Both are safe — neither silently executes anything.
    let output = Command::new(bin())
        .args(["setup", "run", "example"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup run example (no --yes)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("dry-run")
            || combined.contains("Dry-run")
            || combined.contains("privileged"),
        "expected dry-run or privilege gate message, got: {combined}"
    );
}

#[test]
fn setup_run_yes_without_allow_privileged_fails() {
    // --yes without --allow-privileged must be blocked for the example pack
    // (which contains install-dmg and install-pkg steps).
    let output = Command::new(bin())
        .args(["setup", "run", "example", "--yes"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup run example --yes (no allow-privileged)");
    assert!(
        !output.status.success(),
        "expected non-zero: privileged steps without --allow-privileged must abort"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("privileged"),
        "expected 'privileged' in error message, got: {stderr}"
    );
}

#[test]
fn setup_run_yes_with_allow_privileged_defers_to_runner() {
    // With both --yes and --allow-privileged, execution is attempted but the
    // runner core is not yet integrated — must exit nonzero with an informative message.
    let output = Command::new(bin())
        .args(["setup", "run", "example", "--yes", "--allow-privileged"])
        .current_dir(manifest_dir())
        .output()
        .expect("run setup run example --yes --allow-privileged");
    assert!(
        !output.status.success(),
        "expected non-zero: runner not yet integrated"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("runner") || stderr.contains("runner core"),
        "expected runner-not-integrated message, got: {stderr}"
    );
}
