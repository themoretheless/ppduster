use ppduster::automation::{
    run_task, PackTrust, RunOptions, TaskPack, TaskSource,
};
use std::fs;
use std::path::Path;

#[test]
fn loads_bundled_task_pack() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("setup.yaml"),
        r#"
task:
  id: setup-dev
  name: Setup dev
  platform: any
  trust: bundled-only
  steps:
    - id: clone
      type: git-clone
      repo: https://github.com/example/repo.git
      dest: $HOME/Library/Caches/repo
"#,
    )
    .unwrap();

    let pack = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();
    assert_eq!(pack.tasks.len(), 1);
    assert_eq!(pack.tasks[0].id, "setup-dev");
}

#[test]
fn bundled_dev_setup_includes_macos_top_fifty_tasks() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();

    let expected = [
        "macos-top-01-brew-bootstrap",
        "macos-top-02-dotfiles",
        "macos-top-03-system-defaults",
        "macos-top-04-pmset",
        "macos-top-05-toolchains",
        "macos-top-06-launchd",
        "macos-top-07-git-ssh-gpg",
        "macos-top-08-security-baseline",
        "macos-top-09-drift-detection",
        "macos-top-10-rollback-snapshot",
        "macos-top-11-network-identity",
        "macos-top-12-locale-time",
        "macos-top-13-keyboard",
        "macos-top-14-trackpad",
        "macos-top-15-menu-bar",
        "macos-top-16-spotlight",
        "macos-top-17-notifications",
        "macos-top-18-focus",
        "macos-top-19-wallpaper",
        "macos-top-20-screensaver-lock",
        "macos-top-21-privacy-tcc",
        "macos-top-22-filevault",
        "macos-top-23-firewall",
        "macos-top-24-gatekeeper",
        "macos-top-25-softwareupdate",
        "macos-top-26-app-store",
        "macos-top-27-terminal-app",
        "macos-top-28-tmux",
        "macos-top-29-cli-ux",
        "macos-top-30-certificates",
        "macos-top-31-vpn",
        "macos-top-32-proxy",
        "macos-top-33-wifi",
        "macos-top-34-bluetooth",
        "macos-top-35-airdrop",
        "macos-top-36-audio",
        "macos-top-37-default-apps",
        "macos-top-38-browsers",
        "macos-top-39-printers",
        "macos-top-40-fonts",
        "macos-top-41-shortcuts",
        "macos-top-42-applescript",
        "macos-top-43-hammerspoon",
        "macos-top-44-raycast",
        "macos-top-45-hazel",
        "macos-top-46-window-management",
        "macos-top-47-backup",
        "macos-top-48-sync",
        "macos-top-49-observability",
        "macos-top-50-recovery",
    ];

    for id in expected {
        assert!(
            pack.get(id).is_some(),
            "expected bundled task pack to include {id}"
        );
    }
}

#[test]
fn external_pack_requires_flag() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("external.yaml"),
        r#"
task:
  id: ext
  name: External
  platform: any
  trust: external-allowed
  steps:
    - id: dl
      type: download-file
      url: https://example.com/app.tgz
      dest: $HOME/Library/Caches/app.tgz
      checksum:
        sha256: deadbeef
"#,
    )
    .unwrap();

    let err = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::External,
        }],
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--trust-external-packs"));
}

#[test]
fn shell_task_requires_flag_to_run() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("cmd.yaml"),
        r#"
task:
  id: shell-demo
  name: Shell demo
  platform: any
  trust: bundled-only
  steps:
    - id: cmd
      dangerous: true
      type: run-command
      program: bash
      args: ["-lc", "echo hi"]
      shell: allow
"#,
    )
    .unwrap();

    let pack = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();

    let err = run_task(pack.get("shell-demo").unwrap(), &RunOptions::default()).unwrap_err();
    assert!(err.to_string().contains("--allow-shell"));
}

#[test]
fn dev_layout_detection_requires_target_debug_shape() {
    let target_debug = Path::new("/tmp/example/target/debug");
    let cargo_bin = Path::new("/Users/test/.cargo/bin");

    assert_eq!(
        target_debug.file_name().and_then(|n| n.to_str()),
        Some("debug")
    );
    assert_eq!(
        target_debug
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str()),
        Some("target")
    );
    assert_ne!(
        cargo_bin.file_name().and_then(|n| n.to_str()),
        Some("debug")
    );
}
