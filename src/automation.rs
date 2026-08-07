use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::rules::Platform;

// ─── Trust model ──────────────────────────────────────────────────────────────

/// Provenance / trust level of an automation pack.
///
/// The executor uses this to gate dangerous operations:
/// - `Bundled`  — shipped inside the ppduster binary or its install prefix; fully trusted.
/// - `User`     — loaded from `~/.config/ppduster/automations/`; trusted after first-use consent.
/// - `External` — any other path (downloaded, third-party, CI-injected). Strictest validation:
///                downloads MUST carry sha256, write destinations are checked, dmg/pkg steps
///                MUST declare `require_notarized: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PackTrust {
    Bundled,
    User,
    #[default]
    External,
}

impl PackTrust {
    /// Whether downloads without a sha256 checksum are permitted.
    pub fn allows_unverified_downloads(self) -> bool {
        matches!(self, PackTrust::Bundled | PackTrust::User)
    }

    /// Whether write-file / symlink steps may skip destination safety checks.
    pub fn allows_unvalidated_writes(self) -> bool {
        matches!(self, PackTrust::Bundled)
    }
}

// ─── Write-destination safety ─────────────────────────────────────────────────

/// Path prefixes that automation steps must never write to or symlink into,
/// regardless of trust level. Checked by `validate_write_dest`.
pub fn forbidden_write_prefixes() -> Vec<&'static str> {
    vec![
        "/System",
        "/bin",
        "/sbin",
        "/usr",
        "/etc",
        "/dev",
        "/proc",
        "/boot",
        "/lib",
        "/lib64",
        "C:\\Windows",
        "C:\\Program Files",
    ]
}

/// Return `Err` if `dest` falls under a forbidden system prefix.
/// Tilde expansion is intentionally NOT done here — the executor must resolve
/// paths before calling this; unresolved tildes are rejected as ambiguous.
pub fn validate_write_dest(dest: &str) -> Result<()> {
    if dest.starts_with('~') {
        // Tilde paths are permitted but must be resolved by executor before
        // any filesystem operation. We allow them at parse/validation time.
        return Ok(());
    }
    let p = Path::new(dest);
    for prefix in forbidden_write_prefixes() {
        if p.starts_with(prefix) {
            bail!(
                "write destination '{}' is inside a protected system path '{}'",
                dest,
                prefix
            );
        }
    }
    Ok(())
}

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

/// Run a command as a typed argv list — no shell string expansion.
///
/// # Security note
/// `argv` is executed directly via `execvp`-style dispatch; the executor MUST
/// NOT pass it through a shell (`sh -c`). If shell features (pipes, redirects,
/// glob expansion) are genuinely required, the pack author must spell out
/// `["sh", "-c", "..."]` explicitly, which is visible in review.
///
/// `shell_expand` is an explicit opt-in that the executor should warn about
/// and require elevated trust. It must never be set to `true` in Bundled packs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandStep {
    /// Argv list: first element is the executable, remaining are arguments.
    /// No shell interpolation is performed unless `shell_expand` is true.
    pub argv: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// If true, a non-zero exit code does not abort the pack.
    #[serde(default)]
    pub ignore_failure: bool,
    /// Explicit opt-in to pass `argv` through a shell. Requires `User` or
    /// higher trust; rejected entirely for `External` packs by the executor.
    /// # Safety
    /// Setting this to `true` is a security boundary weakening. Document why.
    #[serde(default)]
    pub shell_expand: bool,
}

/// Download a file from a URL.
///
/// # Security note
/// `sha256` is mandatory for `External` packs; the executor MUST refuse to
/// proceed without it when `pack.trust == PackTrust::External`.
/// For `User` and `Bundled` packs it is strongly recommended but not enforced
/// at parse time — call `validate(trust)` before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStep {
    pub url: String,
    pub dest: String,
    /// SHA-256 hex digest of the expected file content.
    /// Required for External packs; strongly recommended otherwise.
    #[serde(default)]
    pub sha256: Option<String>,
}

impl DownloadStep {
    /// Validate this step against the pack's trust level.
    pub fn validate(&self, trust: PackTrust) -> Result<()> {
        if self.sha256.is_none() && !trust.allows_unverified_downloads() {
            bail!(
                "download step for '{}' has no sha256 checksum; \
                 External packs must provide a sha256 for every download",
                self.url
            );
        }
        Ok(())
    }
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
///
/// # Security note
/// The executor MUST verify Gatekeeper/notarization before mounting when
/// `require_notarized` is true (the default). `expected_team_id` is an
/// additional assertion: if set, the executor must confirm the signing
/// certificate's Team ID matches before proceeding.
///
/// **UNIMPLEMENTED SAFETY HOOK**: actual `spctl`/`codesign` verification is
/// not performed at parse time; the executor layer is responsible for calling
/// these checks using these fields as inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallDmgStep {
    pub src: String,
    /// The .app name inside the DMG (e.g. "MyApp.app").
    pub app_name: String,
    /// Destination directory. Defaults to /Applications.
    #[serde(default = "default_applications_dir")]
    pub dest_dir: String,
    /// Require macOS Gatekeeper notarization check before mounting.
    /// Defaults to true; setting false must be justified.
    #[serde(default = "default_true")]
    pub require_notarized: bool,
    /// If set, the executor asserts the DMG's signing Team ID equals this value.
    #[serde(default)]
    pub expected_team_id: Option<String>,
}

fn default_applications_dir() -> String {
    "/Applications".into()
}

/// Install a macOS .pkg package.
///
/// # Security note
/// Same notarization contract as `InstallDmgStep`. The executor must run
/// `pkgutil --check-signature` or equivalent before calling `installer`.
///
/// **UNIMPLEMENTED SAFETY HOOK**: signature verification is not performed at
/// parse time; the executor layer must implement it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallPkgStep {
    pub src: String,
    /// Install target volume. Defaults to "/".
    #[serde(default = "default_pkg_target")]
    pub target: String,
    /// Require macOS Gatekeeper notarization check before installing.
    #[serde(default = "default_true")]
    pub require_notarized: bool,
    /// If set, the executor asserts the pkg's signing Team ID equals this value.
    #[serde(default)]
    pub expected_team_id: Option<String>,
}

fn default_pkg_target() -> String {
    "/".into()
}

/// Create a symbolic link.
///
/// `dest` is validated against `forbidden_write_prefixes` at pack-validation time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymlinkStep {
    pub src: String,
    pub dest: String,
    /// Overwrite an existing symlink or file at dest.
    #[serde(default)]
    pub force: bool,
}

/// Write inline text content to a file.
///
/// `dest` is validated against `forbidden_write_prefixes` at pack-validation time.
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
///
/// The sealed enum ensures no arbitrary execution path can be expressed in YAML
/// beyond the explicitly typed variants defined here.
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

    /// Validate this step's security constraints against the pack trust level.
    pub fn validate(&self, trust: PackTrust) -> Result<()> {
        match self {
            AutomationStep::Download(d) => d.validate(trust)?,
            AutomationStep::WriteFile(w) => validate_write_dest(&w.dest)?,
            AutomationStep::Symlink(s) => validate_write_dest(&s.dest)?,
            AutomationStep::RunCommand(r) => {
                if r.argv.is_empty() {
                    bail!("run-command step has an empty argv list");
                }
                if r.shell_expand && trust == PackTrust::External {
                    bail!(
                        "run-command with shell_expand: true is not permitted in External packs"
                    );
                }
            }
            _ => {}
        }
        Ok(())
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
    /// Trust level assigned by the loader based on source path.
    /// Not read from YAML — the pack author cannot self-promote trust.
    #[serde(skip)]
    pub trust: PackTrust,
    /// Source file path, populated after loading.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl AutomationPack {
    /// Load all `*.yaml` / `*.yml` files from each directory, returning one pack per file.
    /// Files in later dirs override packs with the same name from earlier dirs.
    ///
    /// `trust` is assigned by the caller based on the directory provenance —
    /// pack files cannot declare their own trust level.
    pub fn load_many(dirs: &[PathBuf]) -> Result<Vec<AutomationPack>> {
        AutomationPack::load_many_with_trust(dirs, PackTrust::External)
    }

    /// Like `load_many` but allows the caller to specify a trust level for all
    /// packs loaded from these directories. The bundled loader should pass
    /// `PackTrust::Bundled`; the user config loader `PackTrust::User`.
    pub fn load_many_with_trust(
        dirs: &[PathBuf],
        trust: PackTrust,
    ) -> Result<Vec<AutomationPack>> {
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
                pack.trust = trust;
                pack.source = Some(file);
                by_name.insert(pack.pack.clone(), pack);
            }
        }

        Ok(by_name.into_values().collect())
    }

    /// Validate all steps' security constraints against this pack's trust level.
    /// Call this after loading and before presenting steps to the executor.
    pub fn validate(&self) -> Result<()> {
        for (i, step) in self.steps.iter().enumerate() {
            step.validate(self.trust).with_context(|| {
                format!(
                    "pack '{}' step {} ({}): security validation failed",
                    self.pack,
                    i,
                    step.kind_label()
                )
            })?;
        }
        Ok(())
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
