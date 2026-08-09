use ppduster::automation::{
    run_task, Action, AppStoreOperation, ArchiveFormat, LicenseMethod, LicenseProvider, PackTrust,
    RunOptions, TaskFile, TaskPack, TaskSource,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_ppduster")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/ppduster"))
}

fn write_task(dir: &Path, file: &str, yaml: &str) {
    fs::write(dir.join(file), yaml).unwrap();
}

fn load_bundled_tasks(dir: &Path) -> TaskPack {
    TaskPack::load_many(
        &[TaskSource {
            path: dir.to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap()
}

#[test]
fn task_description_supports_multiline_yaml_and_round_trip() {
    let yaml = r#"
task:
  id: documented-task
  name: Documented task
  description: |
    Checks the current workstation state before changing anything.
    Installs the selected tool only when the check reports it missing.
    Dry-run remains the default and no elevation is requested.
  platform: any
  steps:
    - id: inspect
      name: Inspect the workstation
      type: run-command
      program: /usr/bin/true
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    task.validate().unwrap();
    assert!(task.description.contains('\n'));
    assert!(task.description.contains("before changing anything"));
    assert!(task.description.contains("no elevation is requested"));

    let rendered = serde_yaml::to_string(&task).unwrap();
    let reparsed = serde_yaml::from_str::<ppduster::automation::Task>(&rendered).unwrap();
    assert_eq!(reparsed.description, task.description);
}

#[test]
fn programmatic_task_pack_constructor_preserves_source_provenance() {
    let task = serde_yaml::from_str::<TaskFile>(
        r#"
task:
  id: programmatic-task
  name: Programmatic task
  description: Verifies the supported in-memory task-pack constructor.
  platform: any
  steps:
    - id: inspect
      type: run-command
      program: /usr/bin/true
"#,
    )
    .unwrap()
    .task;
    let source = TaskSource {
        path: PathBuf::from("programmatic-task.yaml"),
        trust: PackTrust::Bundled,
    };

    let pack = TaskPack::from_tasks(vec![task], vec![source], false).unwrap();
    assert_eq!(pack.resolve("programmatic-task").unwrap().steps.len(), 1);

    let error = TaskPack::from_tasks(
        Vec::new(),
        vec![TaskSource {
            path: PathBuf::from("extra-source.yaml"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("count mismatch"),
        "unexpected error: {error}"
    );
}

#[test]
fn task_validation_rejects_blank_description() {
    let yaml = r#"
task:
  id: undocumented-task
  name: Undocumented task
  description: "   "
  platform: any
  steps:
    - id: inspect
      type: run-command
      program: /usr/bin/true
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    let error = task.validate().unwrap_err();
    assert!(error.contains("description"), "unexpected error: {error}");
}

#[test]
fn task_validation_requires_steps_or_scenarios_but_not_both() {
    let both = r#"
task:
  id: ambiguous-task
  name: Ambiguous task
  description: A task body cannot mix direct steps and child scenarios.
  platform: any
  scenarios: [child]
  steps:
    - id: inspect
      type: run-command
      program: /usr/bin/true
"#;
    let neither = r#"
task:
  id: empty-task
  name: Empty task
  description: A task body must contain something to execute.
  platform: any
"#;

    assert!(serde_yaml::from_str::<TaskFile>(both)
        .unwrap()
        .task
        .validate()
        .is_err());
    assert!(serde_yaml::from_str::<TaskFile>(neither)
        .unwrap()
        .task
        .validate()
        .is_err());
}

#[test]
fn task_pack_resolves_child_scenarios_in_declared_order() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "alpha.yaml",
        r#"
task:
  id: alpha
  name: Alpha
  description: Performs the first part of the composed scenario.
  platform: any
  steps:
    - id: prepare
      name: Prepare alpha
      type: run-command
      program: /usr/bin/true
"#,
    );
    write_task(
        dir.path(),
        "beta.yaml",
        r#"
task:
  id: beta
  name: Beta
  description: Performs the second part of the composed scenario.
  platform: any
  steps:
    - id: finish
      name: Finish beta
      type: run-command
      program: /usr/bin/true
"#,
    );
    write_task(
        dir.path(),
        "template.yaml",
        r#"
task:
  id: simple-template
  name: Simple template
  description: Runs alpha first and beta second as one scenario.
  platform: any
  scenarios:
    - alpha
    - beta
"#,
    );

    let pack = load_bundled_tasks(dir.path());
    let resolved = pack.resolve("simple-template").unwrap();
    assert!(resolved.scenarios.is_empty());
    assert_eq!(resolved.included_scenarios(), ["alpha", "beta"]);
    resolved.validate().unwrap();
    let rendered = serde_yaml::to_string(&resolved).unwrap();
    serde_yaml::from_str::<ppduster::automation::Task>(&rendered)
        .unwrap()
        .validate()
        .unwrap();
    let step_ids = resolved
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(step_ids, ["alpha/prepare", "beta/finish"]);
}

#[test]
fn task_pack_resolves_nested_scenarios_in_declared_order() {
    let dir = tempfile::tempdir().unwrap();
    for (file, id, step_id, step_name) in [
        ("first.yaml", "first", "one", "First step"),
        ("second.yaml", "second", "two", "Second step"),
        ("third.yaml", "third", "three", "Third step"),
    ] {
        write_task(
            dir.path(),
            file,
            &format!(
                r#"
task:
  id: {id}
  name: {step_name} scenario
  description: Provides {step_name} for the nested template test.
  platform: any
  steps:
    - id: {step_id}
      name: {step_name}
      type: run-command
      program: /usr/bin/true
"#
            ),
        );
    }
    write_task(
        dir.path(),
        "inner.yaml",
        r#"
task:
  id: inner
  name: Inner template
  description: Groups the first and second child scenarios.
  platform: any
  scenarios: [first, second]
"#,
    );
    write_task(
        dir.path(),
        "outer.yaml",
        r#"
task:
  id: outer
  name: Outer template
  description: Runs the inner template before the final child scenario.
  platform: any
  scenarios: [inner, third]
"#,
    );

    let pack = load_bundled_tasks(dir.path());
    let resolved = pack.resolve("outer").unwrap();
    let step_names = resolved
        .steps
        .iter()
        .map(|step| step.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(step_names, ["First step", "Second step", "Third step"]);
}

#[test]
fn task_pack_load_rejects_unknown_child_scenario() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "broken.yaml",
        r#"
task:
  id: broken-template
  name: Broken template
  description: References a scenario that is not present in the task pack.
  platform: any
  scenarios: [missing-child]
"#,
    );

    let error = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("broken-template"),
        "unexpected error: {error}"
    );
    assert!(error.contains("missing-child"), "unexpected error: {error}");
}

#[test]
fn task_pack_load_rejects_scenario_cycles() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "alpha.yaml",
        r#"
task:
  id: cycle-alpha
  name: Cycle alpha
  description: References the beta half of an invalid cycle.
  platform: any
  scenarios: [cycle-beta]
"#,
    );
    write_task(
        dir.path(),
        "beta.yaml",
        r#"
task:
  id: cycle-beta
  name: Cycle beta
  description: References the alpha half of an invalid cycle.
  platform: any
  scenarios: [cycle-alpha]
"#,
    );

    let error = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("cycle-alpha"), "unexpected error: {error}");
    assert!(error.contains("cycle-beta"), "unexpected error: {error}");
}

#[test]
fn task_pack_load_rejects_duplicate_child_scenarios() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "child.yaml",
        r#"
task:
  id: repeated-child
  name: Repeated child
  description: A valid child that must not be included twice by one template.
  platform: any
  steps:
    - id: inspect
      type: run-command
      program: /usr/bin/true
"#,
    );
    write_task(
        dir.path(),
        "template.yaml",
        r#"
task:
  id: duplicate-template
  name: Duplicate template
  description: Incorrectly includes the same child scenario twice.
  platform: any
  scenarios:
    - repeated-child
    - repeated-child
"#,
    );

    let error = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("duplicate-template"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("repeated-child"),
        "unexpected error: {error}"
    );
}

#[test]
fn task_pack_rejects_a_scenario_repeated_through_diamond_composition() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "shared.yaml",
        r#"
task:
  id: shared
  name: Shared scenario
  description: Performs one side effect that must not be expanded twice.
  platform: any
  steps:
    - id: apply
      type: run-command
      program: /usr/bin/true
"#,
    );
    for branch in ["alpha", "beta"] {
        write_task(
            dir.path(),
            &format!("{branch}.yaml"),
            &format!(
                r#"
task:
  id: {branch}
  name: {branch} branch
  description: Includes the shared scenario through the {branch} branch.
  platform: any
  scenarios: [shared]
"#
            ),
        );
    }
    write_task(
        dir.path(),
        "root.yaml",
        r#"
task:
  id: diamond-root
  name: Diamond root
  description: Incorrectly reaches one shared scenario through two branches.
  platform: any
  scenarios: [alpha, beta]
"#,
    );

    let error = TaskPack::load_many(
        &[TaskSource {
            path: dir.path().to_path_buf(),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("diamond-root"), "unexpected error: {error}");
    assert!(error.contains("shared"), "unexpected error: {error}");
    assert!(
        error.contains("more than once"),
        "unexpected error: {error}"
    );
}

#[test]
fn trusted_template_cannot_include_a_less_trusted_scenario() {
    let bundled = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    write_task(
        bundled.path(),
        "template.yaml",
        r#"
task:
  id: bundled-template
  name: Bundled template
  description: Must not silently inherit executable content from an external pack.
  platform: any
  trust: bundled-only
  scenarios: [external-child]
"#,
    );
    write_task(
        external.path(),
        "child.yaml",
        r#"
task:
  id: external-child
  name: External child
  description: Represents explicitly trusted but lower-provenance executable content.
  platform: any
  trust: external-allowed
  steps:
    - id: apply
      type: run-command
      program: /usr/bin/true
"#,
    );

    let error = TaskPack::load_many(
        &[
            TaskSource {
                path: bundled.path().to_path_buf(),
                trust: PackTrust::Bundled,
            },
            TaskSource {
                path: external.path().to_path_buf(),
                trust: PackTrust::External,
            },
        ],
        true,
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("bundled-template"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("external-child"),
        "unexpected error: {error}"
    );
    assert!(error.contains("less-trusted"), "unexpected error: {error}");
}

#[test]
fn resolved_scenarios_namespace_step_ids_and_preserve_permission_gates() {
    let dir = tempfile::tempdir().unwrap();
    write_task(
        dir.path(),
        "plain.yaml",
        r#"
task:
  id: plain
  name: Plain child
  description: Provides an unprivileged step with a colliding local ID.
  platform: any
  steps:
    - id: inspect
      name: Inspect without elevation
      type: run-command
      program: /usr/bin/true
"#,
    );
    write_task(
        dir.path(),
        "privileged.yaml",
        r#"
task:
  id: privileged
  name: Privileged child
  description: Provides an elevated step with the same local ID.
  platform: any
  steps:
    - id: inspect
      name: Inspect with elevation
      auth: sudo
      allow_elevation: allow
      type: run-command
      program: /usr/bin/true
"#,
    );
    write_task(
        dir.path(),
        "template.yaml",
        r#"
task:
  id: permission-template
  name: Permission template
  description: Combines unprivileged and privileged checks without losing policy metadata.
  platform: any
  scenarios: [plain, privileged]
"#,
    );

    let pack = load_bundled_tasks(dir.path());
    let resolved = pack.resolve("permission-template").unwrap();
    let step_ids = resolved
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(step_ids, ["plain/inspect", "privileged/inspect"]);

    let error = run_task(&resolved, &RunOptions::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("privileged/inspect"),
        "unexpected error: {error}"
    );
    assert!(
        error.contains("--allow-elevation"),
        "unexpected error: {error}"
    );

    let report = run_task(
        &resolved,
        &RunOptions {
            allow_elevation: true,
            ..RunOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.task_name, "Permission template");
    assert!(report.task_description.contains("policy metadata"));
    assert_eq!(report.scenarios, ["plain", "privileged"]);
    let planned_ids = report
        .plans
        .iter()
        .map(|plan| plan.step_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(planned_ids, ["plain/inspect", "privileged/inspect"]);
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
  description: Loads one bundled setup task from a task pack directory.
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
    assert!(
        pack.get("macos-developer-workstation").is_some(),
        "expected bundled task pack to include the developer workstation template"
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
  description: Extracts an archive with an explicit safe format and default size limit.
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
  description: Installs an application through the typed Mac App Store action.
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
  description: Demonstrates that typed App Store steps require explicit elevation.
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
  description: Demonstrates that license secrets are forbidden in scenario files.
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
  description: Demonstrates nested secret-field rejection during task-pack loading.
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
  description: Runs a command that intentionally fails to test the CLI exit status.
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
  description: Plans a typed Mac App Store installation through the setup CLI.
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
fn bundled_tasks_include_dodopizza_package_registry_files() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();

    let task = pack
        .get("dev-dodopizza-package-registries")
        .expect("bundled package registry task");
    assert_eq!(task.steps.len(), 1);

    let report = run_task(task, &RunOptions::default()).unwrap();
    assert_eq!(report.plans.len(), 1);
    assert!(report.plans[0].summary.contains("@dodopizza"));
    assert!(report.plans[0].summary.contains("Dodo.*"));
    assert!(report.plans[0]
        .summary
        .contains("encrypted secrets profile dodopizza-github-packages"));
    assert!(report.plans[0].summary.contains("GITHUB_PACKAGES_USER"));
    assert!(report.plans[0].summary.contains("GITHUB_PACKAGES_TOKEN"));
    assert!(report.plans[0]
        .summary
        .contains("https://npm.pkg.github.com/"));
    assert!(report.plans[0]
        .summary
        .contains("https://api.nuget.org/v3/index.json"));
    assert!(report.plans[0]
        .summary
        .contains("https://nuget.pkg.github.com/dodopizza/index.json"));
    assert!(report.plans[0]
        .summary
        .contains("NuGet <clear/> replaces inherited sources"));
}

#[test]
fn package_registry_cli_writes_only_literal_credential_references() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("package.json"), b"{}\n").unwrap();
    fs::write(dir.path().join("Example.sln"), b"\n").unwrap();
    let empty_bin = dir.path().join("empty-bin");
    fs::create_dir(&empty_bin).unwrap();
    let audit = dir.path().join("audit.jsonl");
    let secret_canary = "do-not-write-this-token-7f38e0";
    let user_canary = "do-not-write-this-user-29c1a4";

    let dry_run = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(dir.path())
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "run",
            "dev-dodopizza-package-registries",
        ])
        .env("GITHUB_PACKAGES_TOKEN", secret_canary)
        .env("GITHUB_PACKAGES_USER", user_canary)
        .env("PATH", &empty_bin)
        .output()
        .unwrap();
    assert!(dry_run.status.success());
    assert!(!dir.path().join(".npmrc").exists());
    assert!(!dir.path().join("NuGet.Config").exists());

    let output = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(dir.path())
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "run",
            "dev-dodopizza-package-registries",
            "--yes",
        ])
        .env("GITHUB_PACKAGES_TOKEN", secret_canary)
        .env("GITHUB_PACKAGES_USER", user_canary)
        .env("PATH", &empty_bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let npmrc = fs::read_to_string(dir.path().join(".npmrc")).unwrap();
    let nuget = fs::read_to_string(dir.path().join("NuGet.Config")).unwrap();
    let second = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(dir.path())
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "run",
            "dev-dodopizza-package-registries",
            "--yes",
        ])
        .env("GITHUB_PACKAGES_TOKEN", secret_canary)
        .env("GITHUB_PACKAGES_USER", user_canary)
        .env("PATH", &empty_bin)
        .output()
        .unwrap();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("satisfied"));
    assert_eq!(
        fs::read_to_string(dir.path().join(".npmrc")).unwrap(),
        npmrc
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("NuGet.Config")).unwrap(),
        nuget
    );

    let report = format!(
        "{}{}{}{}{}{}{}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr),
        fs::read_to_string(&audit).unwrap()
    );

    assert!(npmrc.contains("${GITHUB_PACKAGES_TOKEN}"));
    assert!(nuget.contains("%GITHUB_PACKAGES_USER%"));
    assert!(nuget.contains("%GITHUB_PACKAGES_TOKEN%"));
    for rendered in [&npmrc, &nuget, &report] {
        assert!(!rendered.contains(secret_canary));
        assert!(!rendered.contains(user_canary));
    }
}

#[test]
fn package_registry_conflict_is_redacted_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join("package.json"), b"{}\n").unwrap();
    fs::write(dir.path().join("Example.sln"), b"\n").unwrap();
    let audit = dir.path().join("audit.jsonl");
    let conflict_canary = "existing-secret-must-stay-redacted-913aa2";
    let token_canary = "environment-token-must-stay-redacted-13fe9b";
    let existing = format!(
        "<configuration><!-- {} --></configuration>\n",
        conflict_canary
    );
    fs::write(dir.path().join("NuGet.Config"), &existing).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(dir.path())
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "run",
            "dev-dodopizza-package-registries",
            "--yes",
        ])
        .env("GITHUB_PACKAGES_TOKEN", token_canary)
        .env("GITHUB_PACKAGES_USER", "redacted-user")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!dir.path().join(".npmrc").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("NuGet.Config")).unwrap(),
        existing
    );
    assert!(
        audit.exists(),
        "missing audit log; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let audit_contents = fs::read_to_string(audit).unwrap();
    assert!(audit_contents.contains("\"outcome\":\"failed\""));
    let rendered = format!(
        "{}{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        audit_contents
    );
    for secret in [conflict_canary, token_canary, "redacted-user"] {
        assert!(!rendered.contains(secret));
    }
}

#[cfg(unix)]
#[test]
fn encrypted_package_vault_is_separate_and_redacted_end_to_end() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let vault = vault_dir.join("packages.age");
    let bin = dir.path().join("bin");
    let audit = dir.path().join("audit.jsonl");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(repo.join(".git")).unwrap();
    fs::write(repo.join("package.json"), b"{}\n").unwrap();
    fs::write(repo.join("Example.sln"), b"\n").unwrap();
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&vault_dir).unwrap();
    fs::set_permissions(&vault_dir, fs::Permissions::from_mode(0o700)).unwrap();

    let configure = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(&repo)
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "run",
            "dev-dodopizza-package-registries",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(configure.status.success());

    let username = "vault-user-canary-04d8";
    let token = "vault-token-canary-88ec";
    let password = "vault-password-canary-6c573a";
    let input = serde_json::json!({
        "username": username,
        "token": token,
        "password": password,
        "password_confirmation": password,
    })
    .to_string();
    let mut init = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    init.current_dir(&repo).args([
        "--audit-log",
        audit.to_str().unwrap(),
        "setup",
        "secrets",
        "init",
        "dev-dodopizza-package-registries",
        "--file",
        vault.to_str().unwrap(),
        "--input-json-stdin",
    ]);
    let init_output = output_with_stdin(init, input.as_bytes());
    assert!(
        init_output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&init_output.stdout),
        String::from_utf8_lossy(&init_output.stderr)
    );
    assert!(vault.exists());
    assert!(!repo.join("packages.age").exists());
    assert_eq!(
        fs::metadata(&vault).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&vault_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );

    let fake_npm = bin.join("npm");
    fs::write(
        &fake_npm,
        b"#!/bin/sh\n\
          test \"$1\" = ci || exit 31\n\
          test \"$2\" = --ignore-scripts || exit 36\n\
          test -n \"$GITHUB_PACKAGES_TOKEN\" || exit 32\n\
          test -z \"$GITHUB_PACKAGES_USER\" || exit 33\n\
          test \"$NPM_CONFIG_IGNORE_SCRIPTS\" = true || exit 34\n\
          test -n \"$NPM_CONFIG_USERCONFIG\" || exit 37\n\
          test -n \"$NPM_CONFIG_GLOBALCONFIG\" || exit 38\n\
          test -z \"$npm_config_ignore_scripts\" || exit 39\n\
          test -z \"$npm_config_userconfig\" || exit 40\n\
          test -z \"$NODE_OPTIONS\" || exit 35\n\
          printf 'PACKAGE_SECRET_ENV_PROBE_OK token=%s\\n' \"$GITHUB_PACKAGES_TOKEN\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_npm, fs::Permissions::from_mode(0o700)).unwrap();

    let mut exec = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    exec.current_dir(&repo)
        .env("PATH", &bin)
        .env("NODE_OPTIONS", "--require=untrusted-hook")
        .env("npm_config_ignore_scripts", "false")
        .env("npm_config_userconfig", "/attacker/npmrc")
        .env(
            "npm_config_@dodopizza:registry",
            "https://attacker.invalid/",
        )
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "secrets",
            "exec",
            "dev-dodopizza-package-registries",
            "npm",
            "--file",
            vault.to_str().unwrap(),
            "--password-stdin",
            "--",
            "ci",
        ]);
    let exec_output = output_with_stdin(exec, format!("{password}\n").as_bytes());
    assert!(
        exec_output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&exec_output.stdout),
        String::from_utf8_lossy(&exec_output.stderr)
    );
    assert!(String::from_utf8_lossy(&exec_output.stdout)
        .contains("PACKAGE_SECRET_ENV_PROBE_OK token=[REDACTED]"));

    let fake_dotnet = bin.join("dotnet");
    fs::write(
        &fake_dotnet,
        b"#!/bin/sh\n\
          test \"$1\" = restore || exit 41\n\
          test \"$2\" = --configfile || exit 45\n\
          test \"$3\" = NuGet.Config || exit 46\n\
          test -n \"$GITHUB_PACKAGES_TOKEN\" || exit 42\n\
          test -n \"$GITHUB_PACKAGES_USER\" || exit 43\n\
          test -z \"$DOTNET_STARTUP_HOOKS\" || exit 44\n\
          test -z \"$restoreSources\" || exit 49\n\
          test -z \"$RESTOREADDITIONALPROJECTSOURCES\" || exit 50\n\
          test -z \"$rEsToReCoNfIgFiLe\" || exit 51\n\
          test -z \"$RestoreFallbackFolders\" || exit 52\n\
          test -z \"$restoreAdditionalProjectFallbackFolders\" || exit 53\n\
          test \"$MSBUILDDISABLENODEREUSE\" = 1 || exit 47\n\
          test \"$DOTNET_CLI_USE_MSBUILD_SERVER\" = 0 || exit 48\n\
          printf 'DOTNET_SECRET_ENV_PROBE_OK user=%s token=%s\\n' \"$GITHUB_PACKAGES_USER\" \"$GITHUB_PACKAGES_TOKEN\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_dotnet, fs::Permissions::from_mode(0o700)).unwrap();
    let mut dotnet_exec = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    dotnet_exec
        .current_dir(&repo)
        .env("PATH", &bin)
        .env("DOTNET_STARTUP_HOOKS", "untrusted-hook.dll")
        .env(
            "restoreSources",
            "https://source-override-canary.invalid/v3/index.json",
        )
        .env(
            "RESTOREADDITIONALPROJECTSOURCES",
            "https://additional-source-canary.invalid/v3/index.json",
        )
        .env("rEsToReCoNfIgFiLe", "config-override-canary.xml")
        .env("RestoreFallbackFolders", "fallback-override-canary")
        .env(
            "restoreAdditionalProjectFallbackFolders",
            "additional-fallback-canary",
        )
        .args([
            "--audit-log",
            audit.to_str().unwrap(),
            "setup",
            "secrets",
            "exec",
            "dev-dodopizza-package-registries",
            "dotnet",
            "--file",
            vault.to_str().unwrap(),
            "--password-stdin",
            "--",
            "restore",
        ]);
    let dotnet_output = output_with_stdin(dotnet_exec, format!("{password}\n").as_bytes());
    assert!(
        dotnet_output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&dotnet_output.stdout),
        String::from_utf8_lossy(&dotnet_output.stderr)
    );
    assert!(String::from_utf8_lossy(&dotnet_output.stdout)
        .contains("DOTNET_SECRET_ENV_PROBE_OK user=[REDACTED] token=[REDACTED]"));

    let npmrc = fs::read(repo.join(".npmrc")).unwrap();
    let nuget = fs::read(repo.join("NuGet.Config")).unwrap();
    let ciphertext = fs::read(&vault).unwrap();
    let audit_contents = fs::read(&audit).unwrap();
    let visible = [
        configure.stdout,
        configure.stderr,
        init_output.stdout,
        init_output.stderr,
        exec_output.stdout,
        exec_output.stderr,
        dotnet_output.stdout,
        dotnet_output.stderr,
        npmrc,
        nuget,
        ciphertext,
        audit_contents,
    ]
    .concat();
    for secret in [
        username,
        token,
        password,
        "source-override-canary",
        "additional-source-canary",
        "config-override-canary",
        "fallback-override-canary",
        "additional-fallback-canary",
    ] {
        assert!(
            !visible
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "secret canary leaked into a persisted or rendered surface"
        );
    }
}

#[cfg(unix)]
#[test]
fn encrypted_package_vault_wrong_password_is_generic_and_starts_no_child() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let vault = vault_dir.join("packages.age");
    let bin = dir.path().join("bin");
    let child_marker = dir.path().join("child-ran");
    let audit = dir.path().join("audit.jsonl");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(repo.join(".git")).unwrap();
    fs::write(repo.join("package.json"), b"{}\n").unwrap();
    fs::write(repo.join("Example.csproj"), b"\n").unwrap();
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&vault_dir).unwrap();
    fs::set_permissions(&vault_dir, fs::Permissions::from_mode(0o700)).unwrap();

    let configured = Command::new(env!("CARGO_BIN_EXE_ppduster"))
        .current_dir(&repo)
        .args(["setup", "run", "dev-dodopizza-package-registries", "--yes"])
        .status()
        .unwrap();
    assert!(configured.success());

    let password = "correct-password-canary-181a";
    let input = serde_json::json!({
        "username": "wrong-password-user-canary",
        "token": "wrong-password-token-canary",
        "password": password,
        "password_confirmation": password,
    })
    .to_string();
    let mut init = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    init.current_dir(&repo).args([
        "setup",
        "secrets",
        "init",
        "dev-dodopizza-package-registries",
        "--file",
        vault.to_str().unwrap(),
        "--input-json-stdin",
    ]);
    assert!(output_with_stdin(init, input.as_bytes()).status.success());

    let fake_npm = bin.join("npm");
    fs::write(
        &fake_npm,
        format!("#!/bin/sh\nprintf ran > '{}'\n", child_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&fake_npm, fs::Permissions::from_mode(0o700)).unwrap();

    let wrong_password = "wrong-password-canary-ef97";
    let mut exec = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    exec.current_dir(&repo).env("PATH", &bin).args([
        "--audit-log",
        audit.to_str().unwrap(),
        "setup",
        "secrets",
        "exec",
        "dev-dodopizza-package-registries",
        "npm",
        "--file",
        vault.to_str().unwrap(),
        "--password-stdin",
        "--",
        "ci",
    ]);
    let output = output_with_stdin(exec, format!("{wrong_password}\n").as_bytes());
    assert!(!output.status.success());
    assert!(!child_marker.exists());
    let rendered = [output.stdout, output.stderr, fs::read(&audit).unwrap()].concat();
    let rendered = String::from_utf8_lossy(&rendered);
    assert!(rendered.contains("secret_unlock_failed"));
    for secret in [
        password,
        wrong_password,
        "wrong-password-user-canary",
        "wrong-password-token-canary",
    ] {
        assert!(!rendered.contains(secret));
    }
}

#[cfg(unix)]
#[test]
fn encrypted_vault_rejects_repo_paths_and_audit_aliases_before_reading_secrets() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let vault_dir = dir.path().join("vault");
    let vault = vault_dir.join("packages.age");
    let audit = dir.path().join("safe-audit.jsonl");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(repo.join(".git")).unwrap();
    fs::create_dir(&vault_dir).unwrap();
    fs::set_permissions(&vault_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let secret_input = br#"{"username":"unread-user-canary","token":"unread-token-canary","password":"unread-password-canary","password_confirmation":"unread-password-canary"}"#;

    let mut inside_repo = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    inside_repo.current_dir(&repo).args([
        "--audit-log",
        audit.to_str().unwrap(),
        "setup",
        "secrets",
        "init",
        "dev-dodopizza-package-registries",
        "--file",
        repo.join("packages.age").to_str().unwrap(),
        "--input-json-stdin",
    ]);
    let inside_output = output_with_stdin(inside_repo, secret_input);
    assert!(!inside_output.status.success());
    assert!(!repo.join("packages.age").exists());

    let mut exact_collision = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    exact_collision.current_dir(&repo).args([
        "--audit-log",
        vault.to_str().unwrap(),
        "setup",
        "secrets",
        "init",
        "dev-dodopizza-package-registries",
        "--file",
        vault.to_str().unwrap(),
        "--input-json-stdin",
    ]);
    let collision_output = output_with_stdin(exact_collision, secret_input);
    assert!(!collision_output.status.success());
    assert!(!vault.exists());

    let dangling_audit = dir.path().join("dangling-audit");
    symlink(&vault, &dangling_audit).unwrap();
    let mut dangling_collision = Command::new(env!("CARGO_BIN_EXE_ppduster"));
    dangling_collision.current_dir(&repo).args([
        "--audit-log",
        dangling_audit.to_str().unwrap(),
        "setup",
        "secrets",
        "init",
        "dev-dodopizza-package-registries",
        "--file",
        vault.to_str().unwrap(),
        "--input-json-stdin",
    ]);
    let dangling_output = output_with_stdin(dangling_collision, secret_input);
    assert!(!dangling_output.status.success());
    assert!(!vault.exists());

    let rendered = [
        inside_output.stdout,
        inside_output.stderr,
        collision_output.stdout,
        collision_output.stderr,
        dangling_output.stdout,
        dangling_output.stderr,
        fs::read(audit).unwrap(),
    ]
    .concat();
    let rendered = String::from_utf8_lossy(&rendered);
    for secret in [
        "unread-user-canary",
        "unread-token-canary",
        "unread-password-canary",
    ] {
        assert!(!rendered.contains(secret));
    }
}

fn output_with_stdin(mut command: Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
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
  description: Downloads an external archive only after explicit pack trust is granted.
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
  description: Exercises the explicit shell permission gate for dangerous commands.
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
  description: Preserves Git credential and sudo prerequisites in the generated plan.
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
