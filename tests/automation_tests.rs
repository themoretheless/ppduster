use ppduster::automation::{AutomationPack, AutomationStep, StepOutcome, TaskResult};
use std::path::PathBuf;

// ─── Round-trip parsing ───────────────────────────────────────────────────────

#[test]
fn parses_git_clone_step() {
    let yaml = r#"
pack: test-git
steps:
  - type: git-clone
    url: https://github.com/example/repo.git
    dest: /tmp/repo
    branch: main
    depth: 1
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(pack.pack, "test-git");
    assert_eq!(pack.steps.len(), 1);
    assert_eq!(pack.steps[0].kind_label(), "git-clone");
    if let AutomationStep::GitClone(g) = &pack.steps[0] {
        assert_eq!(g.url, "https://github.com/example/repo.git");
        assert_eq!(g.dest, "/tmp/repo");
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.depth, 1);
    } else {
        panic!("expected GitClone");
    }
}

#[test]
fn parses_brew_install_with_tap() {
    let yaml = r#"
pack: test-brew
steps:
  - type: brew-install
    package: act
    tap: nektos/tap
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::BrewInstall(b) = &pack.steps[0] {
        assert_eq!(b.package, "act");
        assert_eq!(b.tap.as_deref(), Some("nektos/tap"));
    } else {
        panic!("expected BrewInstall");
    }
}

#[test]
fn parses_brew_cask_step() {
    let yaml = r#"
pack: test-cask
steps:
  - type: brew-cask
    package: visual-studio-code
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::BrewCask(c) = &pack.steps[0] {
        assert_eq!(c.package, "visual-studio-code");
    } else {
        panic!("expected BrewCask");
    }
}

#[test]
fn parses_run_command_with_env_and_workdir() {
    let yaml = r#"
pack: test-cmd
steps:
  - type: run-command
    command: cargo
    args: ["test", "--workspace"]
    working_dir: /tmp/myrepo
    env:
      RUST_BACKTRACE: "1"
    ignore_failure: false
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::RunCommand(r) = &pack.steps[0] {
        assert_eq!(r.command, "cargo");
        assert_eq!(r.args, vec!["test", "--workspace"]);
        assert_eq!(r.working_dir.as_deref(), Some("/tmp/myrepo"));
        assert_eq!(r.env.get("RUST_BACKTRACE").map(|s| s.as_str()), Some("1"));
        assert!(!r.ignore_failure);
    } else {
        panic!("expected RunCommand");
    }
}

#[test]
fn parses_download_with_checksum() {
    let yaml = r#"
pack: test-dl
steps:
  - type: download
    url: https://example.com/tool.tar.gz
    dest: /tmp/tool.tar.gz
    sha256: deadbeef1234
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::Download(d) = &pack.steps[0] {
        assert_eq!(d.url, "https://example.com/tool.tar.gz");
        assert_eq!(d.sha256.as_deref(), Some("deadbeef1234"));
    } else {
        panic!("expected Download");
    }
}

#[test]
fn parses_download_without_checksum() {
    let yaml = r#"
pack: test-dl-nochk
steps:
  - type: download
    url: https://example.com/file.zip
    dest: /tmp/file.zip
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::Download(d) = &pack.steps[0] {
        assert!(d.sha256.is_none());
    } else {
        panic!("expected Download");
    }
}

#[test]
fn parses_extract_step() {
    let yaml = r#"
pack: test-extract
steps:
  - type: extract
    src: /tmp/tool.tar.gz
    dest: /usr/local
    strip_components: 1
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::Extract(e) = &pack.steps[0] {
        assert_eq!(e.strip_components, 1);
    } else {
        panic!("expected Extract");
    }
}

#[test]
fn parses_install_dmg_default_dest() {
    let yaml = r#"
pack: test-dmg
steps:
  - type: install-dmg
    src: /tmp/App.dmg
    app_name: App.app
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::InstallDmg(d) = &pack.steps[0] {
        assert_eq!(d.dest_dir, "/Applications");
    } else {
        panic!("expected InstallDmg");
    }
}

#[test]
fn parses_install_dmg_custom_dest() {
    let yaml = r#"
pack: test-dmg-custom
steps:
  - type: install-dmg
    src: /tmp/App.dmg
    app_name: App.app
    dest_dir: ~/Applications
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::InstallDmg(d) = &pack.steps[0] {
        assert_eq!(d.dest_dir, "~/Applications");
    } else {
        panic!("expected InstallDmg");
    }
}

#[test]
fn parses_install_pkg_default_target() {
    let yaml = r#"
pack: test-pkg
steps:
  - type: install-pkg
    src: /tmp/installer.pkg
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::InstallPkg(p) = &pack.steps[0] {
        assert_eq!(p.target, "/");
    } else {
        panic!("expected InstallPkg");
    }
}

#[test]
fn parses_symlink_step() {
    let yaml = r#"
pack: test-symlink
steps:
  - type: symlink
    src: ~/.dotfiles/zshrc
    dest: ~/.zshrc
    force: true
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::Symlink(s) = &pack.steps[0] {
        assert!(s.force);
        assert_eq!(s.dest, "~/.zshrc");
    } else {
        panic!("expected Symlink");
    }
}

#[test]
fn parses_write_file_step() {
    let yaml = r#"
pack: test-write
steps:
  - type: write-file
    dest: /tmp/hello.txt
    content: "hello world\n"
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::WriteFile(w) = &pack.steps[0] {
        assert!(w.content.contains("hello world"));
        assert!(w.create_parents); // default true
    } else {
        panic!("expected WriteFile");
    }
}

#[test]
fn parses_set_env_hint_step() {
    let yaml = r#"
pack: test-env
steps:
  - type: set-env-hint
    var: GOPATH
    value: ~/go
    note: Required for go binaries in PATH
"#;
    let pack: AutomationPack = serde_yaml::from_str(yaml).unwrap();
    if let AutomationStep::SetEnvHint(e) = &pack.steps[0] {
        assert_eq!(e.var, "GOPATH");
        assert_eq!(e.note.as_deref(), Some("Required for go binaries in PATH"));
    } else {
        panic!("expected SetEnvHint");
    }
}

#[test]
fn unknown_step_type_is_error() {
    let yaml = r#"
pack: bad
steps:
  - type: does-not-exist
    foo: bar
"#;
    assert!(serde_yaml::from_str::<AutomationPack>(yaml).is_err());
}

#[test]
fn missing_required_field_is_error() {
    // brew-install requires `package`
    let yaml = r#"
pack: bad
steps:
  - type: brew-install
"#;
    assert!(serde_yaml::from_str::<AutomationPack>(yaml).is_err());
}

// ─── Platform filtering ───────────────────────────────────────────────────────

#[test]
fn applicable_steps_returns_empty_for_wrong_platform() {
    // Force a platform that will never match the host in CI.
    // We pick the platform opposite to what we're running on.
    #[cfg(target_os = "macos")]
    let wrong_platform = "linux";
    #[cfg(not(target_os = "macos"))]
    let wrong_platform = "macos";

    let yaml = format!(
        r#"
pack: platform-test
platform: {wrong_platform}
steps:
  - type: brew-install
    package: git
"#
    );
    let pack: AutomationPack = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(pack.applicable_steps().len(), 0);
}

// ─── load_many ────────────────────────────────────────────────────────────────

#[test]
fn load_many_empty_dir_returns_empty_vec() {
    let tmp = tempfile::tempdir().unwrap();
    let packs = AutomationPack::load_many(&[tmp.path().to_path_buf()]).unwrap();
    assert!(packs.is_empty());
}

#[test]
fn load_many_skips_nonexistent_dir() {
    let nonexistent = PathBuf::from("/tmp/ppduster_nonexistent_12345");
    let packs = AutomationPack::load_many(&[nonexistent]).unwrap();
    assert!(packs.is_empty());
}

#[test]
fn load_many_loads_single_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("mypack.yaml"),
        r#"
pack: mypack
steps:
  - type: brew-cask
    package: iterm2
"#,
    )
    .unwrap();
    let packs = AutomationPack::load_many(&[tmp.path().to_path_buf()]).unwrap();
    assert_eq!(packs.len(), 1);
    assert_eq!(packs[0].pack, "mypack");
    assert!(packs[0].source.is_some());
}

#[test]
fn load_many_later_dir_overrides_same_pack_name() {
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp1.path().join("setup.yaml"),
        r#"
pack: setup
steps:
  - type: brew-install
    package: original
"#,
    )
    .unwrap();
    std::fs::write(
        tmp2.path().join("setup.yaml"),
        r#"
pack: setup
steps:
  - type: brew-install
    package: override
"#,
    )
    .unwrap();
    let packs = AutomationPack::load_many(&[
        tmp1.path().to_path_buf(),
        tmp2.path().to_path_buf(),
    ])
    .unwrap();
    assert_eq!(packs.len(), 1);
    if let AutomationStep::BrewInstall(b) = &packs[0].steps[0] {
        assert_eq!(b.package, "override");
    } else {
        panic!("expected BrewInstall");
    }
}

#[test]
fn load_many_ignores_non_yaml_files() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("readme.txt"), "not yaml").unwrap();
    std::fs::write(tmp.path().join("pack.yaml"), "pack: real\nsteps: []\n").unwrap();
    let packs = AutomationPack::load_many(&[tmp.path().to_path_buf()]).unwrap();
    assert_eq!(packs.len(), 1);
}

#[test]
fn load_many_parse_error_propagates() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("bad.yaml"), "{{ not valid yaml: [").unwrap();
    assert!(AutomationPack::load_many(&[tmp.path().to_path_buf()]).is_err());
}

// ─── TaskResult ───────────────────────────────────────────────────────────────

#[test]
fn task_result_success_and_failure_counts() {
    let mut r = TaskResult::new("my-pack");
    r.push("brew-install", StepOutcome::Success);
    r.push("git-clone", StepOutcome::Failure("timeout".into()));
    r.push("run-command", StepOutcome::Skipped);
    r.push("download", StepOutcome::Success);
    assert_eq!(r.success_count(), 2);
    assert_eq!(r.failure_count(), 1);
    assert_eq!(r.outcomes.len(), 4);
}

#[test]
fn task_result_empty_pack() {
    let r = TaskResult::new("empty");
    assert_eq!(r.success_count(), 0);
    assert_eq!(r.failure_count(), 0);
    assert!(r.outcomes.is_empty());
}

// ─── Sample YAML files ────────────────────────────────────────────────────────

#[test]
fn sample_dev_setup_yaml_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("automations")
        .join("dev-setup.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("could not read {}", path.display()));
    let pack: AutomationPack = serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("parse error in dev-setup.yaml: {e}"));
    assert_eq!(pack.pack, "dev-setup");
    assert!(!pack.steps.is_empty());
}

#[test]
fn sample_brew_bundle_yaml_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("automations")
        .join("brew-bundle.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("could not read {}", path.display()));
    let pack: AutomationPack = serde_yaml::from_str(&text)
        .unwrap_or_else(|e| panic!("parse error in brew-bundle.yaml: {e}"));
    assert_eq!(pack.pack, "brew-bundle");
    assert!(!pack.steps.is_empty());
    // All steps should be BrewCask
    for step in &pack.steps {
        assert_eq!(step.kind_label(), "brew-cask");
    }
}
