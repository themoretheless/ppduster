//! Automation subsystem — CLI adapter stub.
//!
//! This module contains only the minimal adapter surface needed to compile the
//! `automate` CLI commands. The canonical task model, loader, and runner live
//! in the `themoretheless-automation-runner-core` branch and will replace the
//! stubs here once that work merges.
//!
//! **Do not add execution logic here.** That is owned by the runner core and
//! action sessions.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Task model (stub shape — matches runner core canonical YAML) ──────────────

/// A complete automation task loaded from a single YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Human-readable name shown in list/run output.
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
    /// Optional label shown in logs.
    #[serde(default)]
    pub label: Option<String>,
    /// The action this step performs and its parameters.
    #[serde(flatten)]
    pub kind: StepKind,
}

/// All supported step kinds.
///
/// Uses `#[serde(tag = "kind", rename_all = "snake_case")]` so YAML files
/// write `kind: brew_install`, matching the runner core canonical shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    BrewInstall(BrewInstallParams),
    CloneRepo(CloneRepoParams),
    RunCommand(RunCommandParams),
    DownloadFile(DownloadFileParams),
    ExtractArchive(ExtractArchiveParams),
    InstallDmg(InstallDmgParams),
    InstallPkg(InstallPkgParams),
}

impl StepKind {
    /// Short identifier used in dry-run output.
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

    /// Returns true for steps that may require elevated privileges.
    pub fn requires_privilege(&self) -> bool {
        matches!(self, StepKind::InstallDmg(_) | StepKind::InstallPkg(_))
    }

    /// Returns true for steps that run arbitrary external code.
    pub fn is_arbitrary_execution(&self) -> bool {
        matches!(
            self,
            StepKind::RunCommand(_) | StepKind::CloneRepo(_) | StepKind::BrewInstall(_)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrewInstallParams {
    pub packages: Vec<String>,
    #[serde(default)]
    pub cask: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneRepoParams {
    pub url: String,
    pub destination: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub shallow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandParams {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFileParams {
    pub url: String,
    pub destination: String,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractArchiveParams {
    pub source: String,
    pub destination: String,
    #[serde(default)]
    pub strip_components: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDmgParams {
    pub source: String,
    pub app_name: String,
    #[serde(default = "default_applications")]
    pub install_dir: String,
}

fn default_applications() -> String {
    "/Applications".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPkgParams {
    pub source: String,
    #[serde(default = "default_volume")]
    pub target: String,
    #[serde(default = "default_true")]
    pub sudo: bool,
}

fn default_volume() -> String {
    "/".into()
}

fn default_true() -> bool {
    true
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Summary of a task as returned by `list_tasks`.
#[derive(Debug, Clone)]
pub struct TaskMeta {
    /// The task identifier derived from the YAML file stem.
    pub id: String,
    pub name: String,
    pub description: String,
    pub step_count: usize,
}

/// Scan one or more directories for `*.yaml` task files and return metadata.
pub fn list_tasks(dirs: &[PathBuf]) -> Result<Vec<TaskMeta>> {
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            match load_task_file(&path) {
                Ok(task) => {
                    let id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    out.push(TaskMeta {
                        id,
                        name: task.name,
                        description: task.description,
                        step_count: task.steps.len(),
                    });
                }
                Err(e) => {
                    eprintln!("warning: skipping {:?}: {e}", path);
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Load a single task by id from the first matching file in `dirs`.
pub fn load_task(dirs: &[PathBuf], id: &str) -> Result<Task> {
    for dir in dirs {
        let path = dir.join(format!("{id}.yaml"));
        if path.is_file() {
            return load_task_file(&path);
        }
    }
    bail!("task '{id}' not found in {:?}", dirs);
}

fn load_task_file(path: &Path) -> Result<Task> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let task: Task = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
    Ok(task)
}

// ── Dry-run display ───────────────────────────────────────────────────────────

/// Emit a human-readable dry-run preview of a task to stdout.
///
/// No commands are executed. The runner core owns actual execution.
pub fn preview_task(task: &Task) {
    println!("Task: {}", task.name);
    if !task.description.is_empty() {
        println!("  {}", task.description);
    }
    println!();
    for (i, step) in task.steps.iter().enumerate() {
        let label = step
            .label
            .as_deref()
            .unwrap_or(step.kind.kind_label());
        println!("  [{:>2}] {} — {}", i + 1, step.kind.kind_label(), label);
        match &step.kind {
            StepKind::BrewInstall(p) => {
                let cask = if p.cask { " --cask" } else { "" };
                println!("        brew install{cask} {}", p.packages.join(" "));
            }
            StepKind::CloneRepo(p) => {
                let shallow = if p.shallow { " --depth 1" } else { "" };
                let branch = p
                    .branch
                    .as_deref()
                    .map(|b| format!(" -b {b}"))
                    .unwrap_or_default();
                println!("        git clone{shallow}{branch} {} {}", p.url, p.destination);
            }
            StepKind::RunCommand(p) => {
                let args = p.args.join(" ");
                let cwd = p
                    .working_dir
                    .as_deref()
                    .map(|d| format!(" (cwd: {d})"))
                    .unwrap_or_default();
                println!("        {} {args}{cwd}", p.command);
            }
            StepKind::DownloadFile(p) => {
                println!("        curl -L {} -o {}", p.url, p.destination);
            }
            StepKind::ExtractArchive(p) => {
                println!("        extract {} → {}", p.source, p.destination);
            }
            StepKind::InstallDmg(p) => {
                println!("        mount {} → copy {}/{} to {}", p.source, "<dmg>", p.app_name, p.install_dir);
            }
            StepKind::InstallPkg(p) => {
                let sudo = if p.sudo { "sudo " } else { "" };
                println!("        {sudo}installer -pkg {} -target {}", p.source, p.target);
            }
        }
    }
    println!();
    println!("(dry-run — no changes made; re-run with --yes to execute)");
}
