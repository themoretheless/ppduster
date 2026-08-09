use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_ppduster")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/ppduster"))
}

#[test]
fn app_store_help_is_available_without_ppstore_or_a_rule_pack() {
    let workspace = tempfile::tempdir().expect("temporary working directory");
    let output = Command::new(bin())
        .args(["app-store", "--help"])
        .current_dir(workspace.path())
        .env(
            "PPDUSTER_PPSTORE_PATH",
            "relative/path/is-not-used-for-help",
        )
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

#[cfg(unix)]
fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn write_fake_ppstore(directory: &Path, actual_exit: i32) -> (PathBuf, PathBuf) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let executable = directory.join("ppstore");
    let arguments = directory.join("arguments.txt");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$#\" -eq 1 ] && [ \"$1\" = \"--version\" ]; then\n\
           printf 'ppstore 0.1.0\\n'\n\
           exit 0\n\
         fi\n\
         printf '%s\\n' \"$@\" > {}\n\
         printf '%s\\n' '{{\"scanned_roots\":[],\"apps\":[],\"warnings\":[]}}'\n\
         printf 'fake ppstore stderr\\n' >&2\n\
         exit {}\n",
        shell_quote(&arguments),
        actual_exit
    );
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    (executable, arguments)
}

#[cfg(unix)]
fn read_arguments(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
#[test]
fn app_store_proxy_forwards_json_list_and_non_utf8_safe_paths_as_arguments() {
    let workspace = tempfile::tempdir().unwrap();
    let (ppstore, argument_log) = write_fake_ppstore(workspace.path(), 0);
    let app_root = workspace.path().join("Additional Applications");
    let output = Command::new(bin())
        .args(["--output", "json", "app-store", "list", "--app-root"])
        .arg(&app_root)
        .current_dir(workspace.path())
        .env("PPDUSTER_PPSTORE_PATH", ppstore)
        .output()
        .expect("run proxied app-store list");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["apps"].is_array());
    assert_eq!(
        read_arguments(&argument_log),
        [
            "--output",
            "json",
            "list",
            "--app-root",
            &app_root.to_string_lossy(),
        ]
    );
}

#[cfg(unix)]
#[test]
fn app_store_proxy_preserves_dry_run_and_forwards_apply_only_when_requested() {
    let workspace = tempfile::tempdir().unwrap();
    let (ppstore, argument_log) = write_fake_ppstore(workspace.path(), 0);
    let dry_run = Command::new(bin())
        .args([
            "--output",
            "json",
            "app-store",
            "install",
            "497799835",
            "--get",
            "--timeout",
            "45",
        ])
        .current_dir(workspace.path())
        .env("PPDUSTER_PPSTORE_PATH", &ppstore)
        .env("PPDUSTER_APP_STORE_COUNTRY", "ru")
        .output()
        .unwrap();
    assert!(dry_run.status.success());
    let dry_args = read_arguments(&argument_log);
    assert!(!dry_args.iter().any(|arg| arg == "--yes"));
    assert!(dry_args.windows(2).any(|pair| pair == ["--country", "ru"]));
    assert!(dry_args.iter().any(|arg| arg == "--get"));
    assert!(dry_args.windows(2).any(|pair| pair == ["--timeout", "45"]));

    let applied = Command::new(bin())
        .args(["app-store", "install", "497799835", "--yes", "--no-wait"])
        .current_dir(workspace.path())
        .env("PPDUSTER_PPSTORE_PATH", ppstore)
        .output()
        .unwrap();
    assert!(applied.status.success());
    let applied_args = read_arguments(&argument_log);
    assert!(applied_args.iter().any(|arg| arg == "--yes"));
    assert!(applied_args.iter().any(|arg| arg == "--no-wait"));
}

#[cfg(unix)]
#[test]
fn app_store_proxy_rejects_invalid_override_and_propagates_child_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let invalid = Command::new(bin())
        .args(["app-store", "list"])
        .current_dir(workspace.path())
        .env("PPDUSTER_PPSTORE_PATH", "relative/ppstore")
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("absolute executable path"));

    let (ppstore, _) = write_fake_ppstore(workspace.path(), 7);
    let failed = Command::new(bin())
        .args(["app-store", "list"])
        .current_dir(workspace.path())
        .env("PPDUSTER_PPSTORE_PATH", ppstore)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("status"));
}
