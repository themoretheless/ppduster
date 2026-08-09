use ppduster::automation::{
    run_task, Action, AppStoreOperation, ArchiveFormat, AuthPolicy, ElevationPolicy, LicenseMethod,
    LicenseProvider, PackTrust, RunOptions, ScriptInterpreter, TaskFile, TaskPack, TaskSource,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    assert!(
        pack.get("filesystem-basics").is_some(),
        "expected bundled task pack to include the typed filesystem scenario"
    );
}

#[test]
fn bundled_app_store_tasks_do_not_request_elevation() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();

    for task_id in ["app-store-bootstrap", "macos-top-26-app-store"] {
        let task = pack.get(task_id).unwrap();
        assert!(task.steps.iter().all(|step| {
            matches!(step.auth, AuthPolicy::None)
                && matches!(step.allow_elevation, ElevationPolicy::Forbidden)
        }));
    }
}

#[test]
fn bundled_git_scenarios_sync_main_instead_of_stopping_at_repository_presence() {
    let pack = TaskPack::load_many(
        &[TaskSource {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks"),
            trust: PackTrust::Bundled,
        }],
        false,
    )
    .unwrap();

    for (task_id, step_id) in [
        ("dev-brew-bootstrap", "clone-repo"),
        ("macos-top-02-dotfiles", "clone-dotfiles"),
    ] {
        let task = pack.get(task_id).unwrap();
        let step = task.steps.iter().find(|step| step.id == step_id).unwrap();
        assert!(matches!(
            &step.action,
            Action::GitClone {
                branch: Some(branch),
                ..
            } if branch == "main"
        ));
    }
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
fn app_store_install_action_is_typed_and_unprivileged() {
    let yaml = r#"
task:
  id: app-store-demo
  name: App Store demo
  description: Installs an application through the typed Mac App Store action.
  platform: macos
  steps:
    - id: install
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
fn app_store_install_rejects_privilege_declarations() {
    let yaml = r#"
task:
  id: overprivileged-app-store
  name: Overprivileged App Store task
  description: Demonstrates that native App Store steps reject unnecessary privileges.
  platform: macos
  steps:
    - id: install
      auth: sudo
      allow_elevation: allow
      type: app-store-install
      app_id: 497799835
"#;

    let task_file = serde_yaml::from_str::<TaskFile>(yaml).unwrap();
    let err = task_file.task.validate().unwrap_err();
    assert!(err.contains("must not request authentication or elevation"));
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
        .contains("native App Store install download for application 497799835"));
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
fn filesystem_action_schema_round_trips_with_and_expectations() {
    let yaml = r#"
task:
  id: filesystem-schema
  name: Filesystem schema
  description: Exercises typed directory creation and path metadata assertions.
  platform: any
  trust: bundled-only
  steps:
    - id: create
      type: create-directory
      path: $TMPDIR/example/nested
    - id: inspect
      type: inspect-path
      path: $TMPDIR/example/nested
      recursive_size: true
      expect:
        exists: true
        kind: directory
        empty: false
        min_size_bytes: 1
        max_size_bytes: 4096
        modified_at_or_after: "2000-01-01T00:00:00Z"
        modified_at_or_before: "2100-01-01T00:00:00+00:00"
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    task.validate().unwrap();
    assert!(matches!(task.steps[0].action, Action::CreateDirectory(_)));
    assert!(matches!(task.steps[1].action, Action::InspectPath(_)));

    let rendered = serde_yaml::to_string(&task).unwrap();
    let reparsed = serde_yaml::from_str::<ppduster::automation::Task>(&rendered).unwrap();
    reparsed.validate().unwrap();
    let Action::InspectPath(action) = &reparsed.steps[1].action else {
        panic!("expected inspect-path action");
    };
    let expectation = action.expect.as_ref().unwrap();
    assert_eq!(expectation.exists, Some(true));
    assert_eq!(
        expectation.kind,
        Some(ppduster::automation::PathKind::Directory)
    );
    assert_eq!(expectation.min_size_bytes, Some(1));
    assert_eq!(expectation.max_size_bytes, Some(4096));
}

#[test]
fn filesystem_expectation_validation_rejects_contradictions() {
    for (name, expectation, expected_error) in [
        ("empty", "{}", "at least one assertion"),
        (
            "missing-metadata",
            "{ exists: false, kind: file }",
            "cannot be combined",
        ),
        (
            "reversed-size",
            "{ min_size_bytes: 10, max_size_bytes: 9 }",
            "must not exceed",
        ),
        (
            "empty-positive-size",
            "{ empty: true, min_size_bytes: 1 }",
            "positive min_size_bytes",
        ),
        (
            "reversed-time",
            "{ modified_at_or_after: '2100-01-01T00:00:00Z', modified_at_or_before: '2000-01-01T00:00:00Z' }",
            "must not be later",
        ),
    ] {
        let yaml = format!(
            r#"
task:
  id: {name}
  name: Invalid filesystem expectation
  description: Demonstrates validation of contradictory path assertions.
  platform: any
  steps:
    - id: inspect
      type: inspect-path
      path: $TMPDIR/example
      expect: {expectation}
"#
        );
        let task = serde_yaml::from_str::<TaskFile>(&yaml).unwrap().task;
        let error = task.validate().unwrap_err();
        assert!(
            error.contains(expected_error),
            "case {name}: unexpected error: {error}"
        );
    }
}

#[test]
fn inspect_path_rejects_unknown_expectation_fields_and_invalid_dates() {
    for expectation in [
        "unknown_predicate: true",
        "modified_at_or_after: definitely-not-a-date",
    ] {
        let yaml = format!(
            r#"
task:
  id: invalid-inspection
  name: Invalid inspection
  description: Demonstrates strict typed parsing for path expectations.
  platform: any
  steps:
    - id: inspect
      type: inspect-path
      path: $TMPDIR/example
      expect:
        {expectation}
"#
        );
        assert!(serde_yaml::from_str::<TaskFile>(&yaml).is_err());
    }
}

#[test]
fn inspect_path_cli_reports_json_in_dry_run_and_fails_unmet_expectations() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    fs::write(
        dir.path().join("inspect.yaml"),
        format!(
            r#"
task:
  id: inspect-cli
  name: Inspect CLI
  description: Returns typed path metadata without applying filesystem changes.
  platform: any
  trust: external-allowed
  steps:
    - id: inspect
      type: inspect-path
      path: '{}'
      expect:
        exists: false
"#,
            missing.display()
        ),
    )
    .unwrap();

    let success = Command::new(bin())
        .args([
            "--output",
            "json",
            "--trust-external-packs",
            "setup",
            "run",
            "inspect-cli",
            "--tasks-dir",
        ])
        .arg(dir.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run inspect-path dry-run");
    assert!(
        success.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&success.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&success.stdout).unwrap();
    assert_eq!(json["steps"][0]["output"]["type"], "path-metadata");
    assert_eq!(json["steps"][0]["output"]["value"]["exists"], false);

    let yaml_path = dir.path().join("inspect.yaml");
    let text = fs::read_to_string(&yaml_path)
        .unwrap()
        .replace("exists: false", "exists: true");
    fs::write(yaml_path, text).unwrap();
    let failure = Command::new(bin())
        .args([
            "--output",
            "json",
            "--trust-external-packs",
            "setup",
            "run",
            "inspect-cli",
            "--tasks-dir",
        ])
        .arg(dir.path())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run failing inspect-path dry-run");
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("setup task inspect-cli failed"));
}

#[test]
fn run_script_schema_parses_all_interpreters_and_round_trips() {
    let yaml = r#"
task:
  id: script-schema
  name: Script schema
  description: Exercises typed script interpreters and their optional process settings.
  platform: any
  trust: bundled-only
  steps:
    - id: posix-sh
      dangerous: true
      type: run-script
      interpreter: sh
      script: $HOME/.local/share/setup/prepare.sh
    - id: bash-with-options
      dangerous: true
      type: run-script
      interpreter: bash
      script: /opt/example/bootstrap.bash
      args: ["--label", "two words"]
      cwd: $HOME/work
      env:
        SETUP_MODE: safe
    - id: powershell
      dangerous: true
      type: run-script
      interpreter: powershell
      script: '%USERPROFILE%\setup\configure.ps1'
"#;

    let task_file = serde_yaml::from_str::<TaskFile>(yaml).unwrap();
    task_file.task.validate().unwrap();

    let Action::RunScript {
        interpreter,
        script,
        args,
        cwd,
        env,
    } = &task_file.task.steps[0].action
    else {
        panic!("expected a run-script action");
    };
    assert_eq!(*interpreter, ScriptInterpreter::Sh);
    assert_eq!(script, "$HOME/.local/share/setup/prepare.sh");
    assert!(args.is_empty());
    assert!(cwd.is_none());
    assert!(env.is_empty());

    let Action::RunScript {
        interpreter,
        args,
        cwd,
        env,
        ..
    } = &task_file.task.steps[1].action
    else {
        panic!("expected a run-script action");
    };
    assert_eq!(*interpreter, ScriptInterpreter::Bash);
    assert_eq!(args, &["--label", "two words"]);
    assert_eq!(cwd.as_deref(), Some("$HOME/work"));
    assert_eq!(env.get("SETUP_MODE").map(String::as_str), Some("safe"));

    let Action::RunScript { interpreter, .. } = &task_file.task.steps[2].action else {
        panic!("expected a run-script action");
    };
    assert_eq!(*interpreter, ScriptInterpreter::PowerShell);

    let rendered = serde_yaml::to_string(&task_file).unwrap();
    assert!(rendered.contains("interpreter: powershell"));
    let reparsed = serde_yaml::from_str::<TaskFile>(&rendered).unwrap();
    reparsed.task.validate().unwrap();
    assert!(matches!(
        reparsed.task.steps[0].action,
        Action::RunScript {
            interpreter: ScriptInterpreter::Sh,
            ..
        }
    ));
    assert!(matches!(
        reparsed.task.steps[1].action,
        Action::RunScript {
            interpreter: ScriptInterpreter::Bash,
            ..
        }
    ));
    assert!(matches!(
        reparsed.task.steps[2].action,
        Action::RunScript {
            interpreter: ScriptInterpreter::PowerShell,
            ..
        }
    ));
}

#[test]
fn run_script_rejects_inline_source() {
    let yaml = r#"
task:
  id: inline-script
  name: Inline script
  description: Demonstrates that run-script accepts a file path rather than inline source.
  platform: any
  steps:
    - id: inline
      dangerous: true
      type: run-script
      interpreter: bash
      script: |
        echo "this must remain in a script file"
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    let error = task.validate().unwrap_err();
    assert!(error.contains("file path"), "unexpected error: {error}");
    assert!(error.contains("not inline"), "unexpected error: {error}");
}

#[test]
fn run_script_requires_dangerous_declaration() {
    let yaml = r#"
task:
  id: undeclared-script-risk
  name: Undeclared script risk
  description: Demonstrates that every script step must declare its dangerous execution risk.
  platform: any
  steps:
    - id: script
      type: run-script
      interpreter: sh
      script: /opt/example/setup.sh
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    let error = task.validate().unwrap_err();
    assert!(
        error.contains("not marked dangerous"),
        "unexpected error: {error}"
    );
}

#[test]
fn run_script_requires_allow_shell_permission() {
    let yaml = r#"
task:
  id: script-permission
  name: Script permission
  description: Exercises the explicit shell permission gate for typed script execution.
  platform: any
  trust: bundled-only
  steps:
    - id: script
      dangerous: true
      type: run-script
      interpreter: bash
      script: $HOME/.local/share/setup/bootstrap.sh
"#;

    let task = serde_yaml::from_str::<TaskFile>(yaml).unwrap().task;
    task.validate().unwrap();

    let error = run_task(&task, &RunOptions::default()).unwrap_err();
    assert!(error.to_string().contains("--allow-shell"));

    let report = run_task(
        &task,
        &RunOptions {
            allow_shell: true,
            ..RunOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.plans.len(), 1);
    assert!(report.plans[0].summary.contains("Bash"));
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
