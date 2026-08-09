use ppduster::automation::{
    run_task, Action, AppStoreOperation, ArchiveFormat, LicenseMethod, LicenseProvider, PackTrust,
    RunOptions, TaskFile, TaskPack, TaskSource,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_ppduster")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/ppduster"))
}

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

    assert!(
        pack.get("lightburn-install-activate").is_some(),
        "expected bundled task pack to include the LightBurn scenario"
    );
    assert!(
        pack.get("bambu-studio-install").is_some(),
        "expected bundled task pack to include the Bambu Studio scenario"
    );
    assert!(
        pack.get("app-store-bootstrap").is_some(),
        "expected bundled task pack to include the App Store bootstrap scenario"
    );
}

#[test]
fn bambu_studio_task_uses_dynamic_release_channel() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();
    let task = pack.get("bambu-studio-install").unwrap();

    assert_eq!(task.steps.len(), 2);
    assert!(matches!(
        &task.steps[0].action,
        Action::MacosRequirements { minimum_version, .. } if minimum_version == "10.15"
    ));
    assert!(matches!(
        &task.steps[1].action,
        Action::BambuStudioRelease(action)
            if action.channel == ppduster::automation::ReleaseChannel::Release
    ));
}

#[test]
fn extract_archive_action_supports_explicit_format_and_safe_default_limit() {
    let yaml = r#"
task:
  id: unpack-demo
  name: Unpack demo
  steps:
    - id: unpack
      type: extract-archive
      src: $HOME/Library/Caches/input.tar.xz
      dest: $HOME/Library/Caches/output
      format: tar-xz
"#;
    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    task.validate().unwrap();
    assert!(matches!(
        &task.steps[0].action,
        Action::ExtractArchive {
            format: ArchiveFormat::TarXz,
            max_unpacked_bytes: 10_737_418_240,
            ..
        }
    ));
}

#[test]
fn app_store_install_action_is_typed_and_requires_elevation() {
    let yaml = r#"
task:
  id: app-store-demo
  name: App Store demo
  platform: macos
  steps:
    - id: install
      auth: sudo
      allow_elevation: allow
      type: app-store-install
      app_id: 497799835
      operation: install
"#;

    let task_file = serde_yaml::from_str::<TaskFile>(yaml).unwrap();
    task_file.task.validate().unwrap();
    assert!(matches!(
        &task_file.task.steps[0].action,
        Action::AppStoreInstall(action)
            if action.app_id == 497799835
                && action.operation == AppStoreOperation::Install
    ));
}

#[test]
fn app_store_install_rejects_missing_elevation_declaration() {
    let yaml = r#"
task:
  id: unsafe-app-store
  name: Unsafe App Store task
  platform: macos
  steps:
    - id: install
      type: app-store-install
      app_id: 497799835
"#;

    let task_file = serde_yaml::from_str::<TaskFile>(yaml).unwrap();
    let err = task_file.task.validate().unwrap_err();
    assert!(err.contains("auth: sudo plus allow_elevation: allow"));
}

#[test]
fn lightburn_task_downloads_installs_then_uses_vendor_ui() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();
    let task = pack.get("lightburn-install-activate").unwrap();

    assert_eq!(task.steps.len(), 4);
    assert!(matches!(
        &task.steps[0].action,
        Action::MacosRequirements { .. }
    ));
    assert!(matches!(&task.steps[1].action, Action::DownloadFile { .. }));
    assert!(matches!(
        &task.steps[2].action,
        Action::InstallDmg {
            identity: Some(identity),
            ..
        } if identity.bundle_identifier == "com.LightBurnSoftware.LightBurn"
            && identity.team_identifier == "UWZQ3LL82C"
            && identity.version == "2.1.03"
    ));
    assert!(matches!(
        &task.steps[3].action,
        Action::ActivateLicense(action)
            if action.provider == LicenseProvider::LightBurn
                && action.method == LicenseMethod::VendorUi
    ));

    let rendered = serde_yaml::to_string(task).unwrap();
    assert!(!rendered.contains("license_key"));
    assert!(!rendered.contains("license-key"));
}

#[test]
fn activate_license_rejects_embedded_secret_fields() {
    let yaml = r#"
task:
  id: unsafe-license
  name: Unsafe license
  platform: macos
  steps:
    - id: activate
      type: activate-license
      provider: light-burn
      method: vendor-ui
      license_key: CANARY-SECRET
"#;

    let err = serde_yaml::from_str::<TaskFile>(yaml).unwrap_err();
    assert!(err.to_string().contains("unknown field"));
    assert!(!format!("{err:?}").contains("CANARY-SECRET"));
}

#[test]
fn task_pack_rejects_license_key_fields_at_any_yaml_level() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("unsafe-license.yaml"),
        r#"
task:
  id: unsafe-license
  name: Unsafe license
  platform: any
  steps:
    - id: activate
      check:
        license-key: CANARY-NESTED-SECRET
      type: activate-license
      provider: light-burn
      method: vendor-ui
"#,
    )
    .unwrap();

    let err = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err();

    assert!(err.to_string().contains("forbidden field license-key"));
    assert!(!format!("{err:?}").contains("CANARY-NESTED-SECRET"));
}

#[test]
fn setup_cli_returns_failure_when_an_applied_step_fails() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("failing.yaml"),
        r#"
task:
  id: failing-setup
  name: Failing setup
  platform: any
  trust: external-allowed
  steps:
    - id: fail
      type: run-command
      program: /usr/bin/false
"#,
    )
    .unwrap();

    let output = Command::new(bin())
        .args([
            "--trust-external-packs",
            "setup",
            "run",
            "failing-setup",
            "--yes",
            "--tasks-dir",
        ])
        .arg(dir.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run failing setup task");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("setup task failing-setup failed"));
}

#[test]
fn setup_cli_plans_typed_app_store_install() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("app-store.yaml"),
        r#"
task:
  id: app-store-cli-demo
  name: App Store CLI demo
  platform: macos
  trust: external-allowed
  steps:
    - id: install-xcode
      auth: sudo
      allow_elevation: allow
      type: app-store-install
      app_id: 497799835
      operation: install
"#,
    )
    .unwrap();

    let output = Command::new(bin())
        .args([
            "--trust-external-packs",
            "setup",
            "run",
            "app-store-cli-demo",
            "--allow-elevation",
            "--tasks-dir",
        ])
        .arg(dir.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("plan App Store setup task");

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("mas install Mac App Store application 497799835"));
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

#[test]
fn task_pack_parses_auth_prerequisites() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("auth.yaml"),
        r#"
task:
  id: auth-demo
  name: Auth demo
  platform: any
  trust: bundled-only
  steps:
    - id: clone
      auth: git-credential
      type: git-clone
      repo: https://github.com/example/repo.git
      dest: $HOME/Library/Caches/repo
    - id: inspect
      auth: sudo
      allow_elevation: allow
      type: run-command
      program: sudo
      args: ["true"]
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
    let report = run_task(
        pack.get("auth-demo").unwrap(),
        &RunOptions {
            allow_elevation: true,
            ..RunOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.plans.len(), 2);
    assert_eq!(report.plans[0].prerequisites.len(), 1);
    assert_eq!(report.plans[1].prerequisites.len(), 1);
}
