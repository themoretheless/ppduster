use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::rules::Platform;

// ─── Step variants ────────────────────────────────────────────────────────────

/// Clone a git repository to a local destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCloneStep {
    pub url: String,
    pub dest: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Shallow clone depth (0 = full).
    #[serde(default)]
    pub depth: u32,
}

/// Install a Homebrew formula.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrewInstallStep {
    pub package: String,
    /// Optional tap to add before installing (e.g. "user/repo").
    #[serde(default)]
    pub tap: Option<String>,
}

/// Install a Homebrew cask (GUI app).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrewCaskStep {
    pub package: String,
}

/// Run an arbitrary command with optional args, working dir, and env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandStep {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// If true, a non-zero exit code does not abort the pack.
    #[serde(default)]
    pub ignore_failure: bool,
}

/// Download a file from a URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStep {
    pub url: String,
    pub dest: String,
    /// Optional SHA-256 hex digest to verify after download.
    #[serde(default)]
    pub sha256: Option<String>,
}

/// Extract an archive (tar, zip, etc.) to a destination directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractStep {
    pub src: String,
    pub dest: String,
    /// Number of leading path components to strip (like tar --strip-components).
    #[serde(default)]
    pub strip_components: u32,
}

/// Mount a DMG and copy the contained .app bundle to /Applications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDmgStep {
    pub src: String,
    /// The .app name inside the DMG (e.g. "MyApp.app").
    pub app_name: String,
    /// Destination directory. Defaults to /Applications.
    #[serde(default = "default_applications_dir")]
    pub dest_dir: String,
}

fn default_applications_dir() -> String {
    "/Applications".into()
}

/// Install a macOS .pkg package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPkgStep {
    pub src: String,
    /// Install target volume. Defaults to "/".
    #[serde(default = "default_pkg_target")]
    pub target: String,
}

fn default_pkg_target() -> String {
    "/".into()
}

/// Create a symbolic link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymlinkStep {
    pub src: String,
    pub dest: String,
    /// Overwrite an existing symlink or file at dest.
    #[serde(default)]
    pub force: bool,
}

/// Write inline text content to a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileStep {
    pub dest: String,
    pub content: String,
    /// Create parent directories if they don't exist.
    #[serde(default = "default_true")]
    pub create_parents: bool,
}

fn default_true() -> bool {
    true
}

/// Document an environment variable that should be set (no execution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEnvHintStep {
    pub var: String,
    pub value: String,
    /// Human-readable note for why this variable is needed.
    #[serde(default)]
    pub note: Option<String>,
}

// ─── Tagged step enum ─────────────────────────────────────────────────────────

/// A single automation step. The `type` field in YAML selects the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AutomationStep {
    GitClone(GitCloneStep),
    BrewInstall(BrewInstallStep),
    BrewCask(BrewCaskStep),
    RunCommand(RunCommandStep),
    Download(DownloadStep),
    Extract(ExtractStep),
    InstallDmg(InstallDmgStep),
    InstallPkg(InstallPkgStep),
    Symlink(SymlinkStep),
    WriteFile(WriteFileStep),
    SetEnvHint(SetEnvHintStep),
}

impl AutomationStep {
    /// A short human-readable label for display and logging.
    pub fn kind_label(&self) -> &'static str {
        match self {
            AutomationStep::GitClone(_) => "git-clone",
            AutomationStep::BrewInstall(_) => "brew-install",
            AutomationStep::BrewCask(_) => "brew-cask",
            AutomationStep::RunCommand(_) => "run-command",
            AutomationStep::Download(_) => "download",
            AutomationStep::Extract(_) => "extract",
            AutomationStep::InstallDmg(_) => "install-dmg",
            AutomationStep::InstallPkg(_) => "install-pkg",
            AutomationStep::Symlink(_) => "symlink",
            AutomationStep::WriteFile(_) => "write-file",
            AutomationStep::SetEnvHint(_) => "set-env-hint",
        }
    }
}

// ─── Pack / task types ────────────────────────────────────────────────────────

/// A named automation pack loaded from a single YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationPack {
    /// Short identifier (e.g. "dev-setup").
    pub pack: String,
    #[serde(default)]
    pub description: String,
    /// Platform filter; defaults to Any.
    #[serde(default)]
    pub platform: Platform,
    /// Ordered list of steps to execute.
    #[serde(default)]
    pub steps: Vec<AutomationStep>,
    /// Source file path, populated after loading.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl AutomationPack {
    /// Load all `*.yaml` / `*.yml` files from each directory, returning one pack per file.
    /// Files in later dirs override packs with the same name from earlier dirs.
    pub fn load_many(dirs: &[PathBuf]) -> Result<Vec<AutomationPack>> {
        let mut by_name: BTreeMap<String, AutomationPack> = BTreeMap::new();

        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(dir)
                .with_context(|| format!("read automations dir {}", dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x == "yaml" || x == "yml")
                        .unwrap_or(false)
                })
                .collect();
            files.sort();
            for file in files {
                let text = fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                let mut pack: AutomationPack = serde_yaml::from_str(&text)
                    .with_context(|| format!("parse automation pack {}", file.display()))?;
                pack.source = Some(file);
                by_name.insert(pack.pack.clone(), pack);
            }
        }

        Ok(by_name.into_values().collect())
    }

    /// Return steps that apply to the current host platform.
    pub fn applicable_steps(&self) -> &[AutomationStep] {
        if self.platform.matches_host() {
            &self.steps
        } else {
            &[]
        }
    }
}

// ─── Result types (for future executor) ──────────────────────────────────────

/// Outcome of executing a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepOutcome {
    /// Step was not executed (e.g. wrong platform, dry-run).
    Skipped,
    /// Step completed successfully.
    Success,
    /// Step failed with a message.
    Failure(String),
}

/// Result of running an entire automation pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub pack: String,
    pub outcomes: Vec<(String, StepOutcome)>,
}

impl TaskResult {
    pub fn new(pack: &str) -> Self {
        TaskResult {
            pack: pack.to_owned(),
            outcomes: Vec::new(),
        }
    }

    pub fn push(&mut self, kind: impl Into<String>, outcome: StepOutcome) {
        self.outcomes.push((kind.into(), outcome));
    }

    pub fn success_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, StepOutcome::Success))
            .count()
    }

    pub fn failure_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, StepOutcome::Failure(_)))
            .count()
    }
}
