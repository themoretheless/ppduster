use ppduster::automation::{
    run_task, PackTrust, RunOptions, TaskPack, TaskSource,
};
use std::fs;

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
