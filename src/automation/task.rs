//! Declarative automation task data model.
//!
//! Designed to round-trip cleanly with YAML task files. A task file contains
//! a list of steps; each step carries a `kind` discriminant plus its
//! kind-specific parameters. Unknown keys are ignored by serde so future
//! extensions don't break older runners.
//!
//! Example YAML shape:
//! ```yaml
//! name: "Set up dev environment"
//! description: "Clone repos, install tools, run bootstrap"
//! steps:
//!   - kind: brew_install
//!     packages: [git, ripgrep]
//!   - kind: clone_repo
//!     url: https://github.com/example/repo.git
//!     destination: ~/src/repo
//!   - kind: run_command
//!     command: make
//!     args: [bootstrap]
//!     working_dir: ~/src/repo
//!   - kind: download_file
//!     url: https://example.com/archive.tar.gz
//!     destination: /tmp/archive.tar.gz
//!   - kind: extract_archive
//!     source: /tmp/archive.tar.gz
//!     destination: /tmp/extracted
//!   - kind: install_dmg
//!     source: /tmp/App.dmg
//!     app_name: App.app
//!   - kind: install_pkg
//!     source: /tmp/package.pkg
//! ```

use serde::{Deserialize, Serialize};

/// A complete automation task, typically loaded from a single YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Human-readable task name shown in run reports.
    pub name: String,
    /// Optional description of what this task accomplishes.
    #[serde(default)]
    pub description: String,
    /// Ordered list of steps to execute.
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// A single executable step within a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Optional label shown in logs; defaults to the step kind name if absent.
    #[serde(default)]
    pub label: Option<String>,
    /// The action this step performs plus its parameters.
    #[serde(flatten)]
    pub kind: StepKind,
}

/// All supported step kinds with their parameters.
///
/// `#[serde(tag = "kind", rename_all = "snake_case")]` makes YAML/JSON use a
/// `kind: brew_install` discriminant field — matching the natural declarative
/// shape without a wrapper object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    /// Install one or more Homebrew packages.
    BrewInstall(BrewInstallParams),
    /// Clone a git repository to a local path.
    CloneRepo(CloneRepoParams),
    /// Run an arbitrary shell command.
    RunCommand(RunCommandParams),
    /// Download a file from a URL to a local path.
    DownloadFile(DownloadFileParams),
    /// Extract a compressed archive (tar.gz, zip, etc.) to a directory.
    ExtractArchive(ExtractArchiveParams),
    /// Mount a .dmg and copy the bundled .app to /Applications.
    InstallDmg(InstallDmgParams),
    /// Install a macOS .pkg package.
    InstallPkg(InstallPkgParams),
}

impl StepKind {
    /// Short label used in logs and reports.
    pub fn kind_label(&self) -> &'static str {
        match self {
            StepKind::BrewInstall(_) => "brew_install",
            StepKind::CloneRepo(_) => "clone_repo",
            StepKind::RunCommand(_) => "run_command",
            StepKind::DownloadFile(_) => "download_file",
            StepKind::ExtractArchive(_) => "extract_archive",
            StepKind::InstallDmg(_) => "install_dmg",
            StepKind::InstallPkg(_) => "install_pkg",
        }
    }
}

// ── Per-kind parameter structs ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrewInstallParams {
    /// Package names to pass to `brew install`.
    pub packages: Vec<String>,
    /// Pass `--cask` to brew install.
    #[serde(default)]
    pub cask: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneRepoParams {
    /// Remote URL to clone.
    pub url: String,
    /// Local destination path (supports `~` expansion).
    pub destination: String,
    /// Optional single branch name.
    #[serde(default)]
    pub branch: Option<String>,
    /// Pass `--depth 1` for a shallow clone.
    #[serde(default)]
    pub shallow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandParams {
    /// Executable to run (looked up via PATH).
    pub command: String,
    /// Arguments to pass.
    #[serde(default)]
    pub args: Vec<String>,
    /// Override the working directory for this command.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Environment variables to set for this command only.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFileParams {
    /// Source URL.
    pub url: String,
    /// Destination path. Parent directories are created if needed.
    pub destination: String,
    /// Overwrite an existing file at the destination.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractArchiveParams {
    /// Path to the archive file.
    pub source: String,
    /// Directory to extract into. Created if missing.
    pub destination: String,
    /// Strip this many leading path components (like `tar --strip-components`).
    #[serde(default)]
    pub strip_components: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDmgParams {
    /// Path to the .dmg file.
    pub source: String,
    /// Name of the .app bundle inside the DMG.
    pub app_name: String,
    /// Destination directory; defaults to /Applications.
    #[serde(default = "default_applications")]
    pub install_dir: String,
}

fn default_applications() -> String {
    "/Applications".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPkgParams {
    /// Path to the .pkg file.
    pub source: String,
    /// Installation target volume; defaults to `/`.
    #[serde(default = "default_volume")]
    pub target: String,
    /// Use `sudo` when invoking installer.
    #[serde(default = "default_true")]
    pub sudo: bool,
}

fn default_volume() -> String {
    "/".into()
}

fn default_true() -> bool {
    true
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"
name: "Test task"
description: "Verify round-trip"
steps:
  - kind: run_command
    command: echo
    args: ["hello"]
  - kind: download_file
    url: https://example.com/file.tar.gz
    destination: /tmp/file.tar.gz
  - kind: extract_archive
    source: /tmp/file.tar.gz
    destination: /tmp/out
  - kind: brew_install
    packages: [git]
  - kind: clone_repo
    url: https://github.com/example/repo.git
    destination: /tmp/repo
  - kind: install_dmg
    source: /tmp/App.dmg
    app_name: App.app
  - kind: install_pkg
    source: /tmp/pkg.pkg
"#;

    #[test]
    fn round_trip_yaml() {
        let task: Task = serde_yaml::from_str(SAMPLE_YAML).expect("parse failed");
        assert_eq!(task.name, "Test task");
        assert_eq!(task.steps.len(), 7);
        // check kind labels
        assert_eq!(task.steps[0].kind.kind_label(), "run_command");
        assert_eq!(task.steps[1].kind.kind_label(), "download_file");
        assert_eq!(task.steps[2].kind.kind_label(), "extract_archive");
        assert_eq!(task.steps[3].kind.kind_label(), "brew_install");
        assert_eq!(task.steps[4].kind.kind_label(), "clone_repo");
        assert_eq!(task.steps[5].kind.kind_label(), "install_dmg");
        assert_eq!(task.steps[6].kind.kind_label(), "install_pkg");
    }

    #[test]
    fn run_command_params() {
        let yaml = r#"
name: t
steps:
  - kind: run_command
    command: make
    args: [bootstrap]
    working_dir: /tmp
    env:
      FOO: bar
"#;
        let task: Task = serde_yaml::from_str(yaml).unwrap();
        if let StepKind::RunCommand(p) = &task.steps[0].kind {
            assert_eq!(p.command, "make");
            assert_eq!(p.args, vec!["bootstrap"]);
            assert_eq!(p.working_dir.as_deref(), Some("/tmp"));
            assert_eq!(p.env.get("FOO").map(String::as_str), Some("bar"));
        } else {
            panic!("wrong kind");
        }
    }
}
