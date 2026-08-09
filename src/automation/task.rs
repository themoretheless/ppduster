use crate::rules::Platform;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustRequirement {
    #[default]
    BundledOnly,
    UserConfigAllowed,
    ExternalAllowed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellMode {
    #[default]
    Forbidden,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptInterpreter {
    #[serde(rename = "sh")]
    Sh,
    #[serde(rename = "bash")]
    Bash,
    #[serde(rename = "powershell", alias = "pwsh", alias = "power-shell")]
    PowerShell,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElevationPolicy {
    #[default]
    Forbidden,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Checksum {
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Check {
    #[serde(default)]
    pub path_exists: Option<PathBuf>,
    #[serde(default)]
    pub command_succeeds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Assertions evaluated against one atomic path inspection.
///
/// Every populated field is combined with logical AND. This is intentionally
/// separate from `Check`, whose existing meaning is "the mutating step is
/// already satisfied" rather than "fail when this assertion is false".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathExpectation {
    #[serde(default)]
    pub exists: Option<bool>,
    #[serde(default)]
    pub kind: Option<PathKind>,
    #[serde(default)]
    pub empty: Option<bool>,
    #[serde(default)]
    pub min_size_bytes: Option<u64>,
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    #[serde(default)]
    pub modified_at_or_after: Option<DateTime<Utc>>,
    #[serde(default)]
    pub modified_at_or_before: Option<DateTime<Utc>>,
}

impl PathExpectation {
    fn has_metadata_assertion(&self) -> bool {
        self.kind.is_some()
            || self.empty.is_some()
            || self.min_size_bytes.is_some()
            || self.max_size_bytes.is_some()
            || self.modified_at_or_after.is_some()
            || self.modified_at_or_before.is_some()
    }

    fn is_empty(&self) -> bool {
        self.exists.is_none() && !self.has_metadata_assertion()
    }

    fn validate(&self, step_id: &str) -> Result<(), String> {
        if self.is_empty() {
            return Err(format!(
                "step {} inspect-path expect must contain at least one assertion",
                step_id
            ));
        }
        if matches!(self.exists, Some(false)) && self.has_metadata_assertion() {
            return Err(format!(
                "step {} inspect-path expect.exists: false cannot be combined with metadata assertions",
                step_id
            ));
        }
        if let (Some(minimum), Some(maximum)) = (self.min_size_bytes, self.max_size_bytes) {
            if minimum > maximum {
                return Err(format!(
                    "step {} inspect-path min_size_bytes must not exceed max_size_bytes",
                    step_id
                ));
            }
        }
        if let (Some(after), Some(before)) = (
            self.modified_at_or_after.as_ref(),
            self.modified_at_or_before.as_ref(),
        ) {
            if after > before {
                return Err(format!(
                    "step {} inspect-path modified_at_or_after must not be later than modified_at_or_before",
                    step_id
                ));
            }
        }
        if matches!(self.empty, Some(true)) && self.min_size_bytes.is_some_and(|size| size > 0) {
            return Err(format!(
                "step {} inspect-path empty: true cannot require a positive min_size_bytes",
                step_id
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDirectoryAction {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectPathAction {
    pub path: String,
    /// Recursively total regular-file bytes for a directory. Symlinks are not
    /// followed. File sizes are always reported regardless of this setting.
    #[serde(default)]
    pub recursive_size: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<PathExpectation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFile {
    pub task: Task,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub platform: Platform,
    #[serde(default)]
    pub trust: TrustRequirement,
    /// Other scenarios included by this reusable template, in execution order.
    ///
    /// A task definition contains either `scenarios` or `steps`. `TaskPack::resolve`
    /// recursively expands scenario references into a flat, policy-checkable task
    /// immediately before execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "includes")]
    pub scenarios: Vec<String>,
    /// Root scenario references retained after `TaskPack::resolve` flattens a
    /// template. This is runtime provenance only and is never written to YAML.
    #[serde(skip)]
    pub resolved_scenarios: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub auth: AuthPolicy,
    #[serde(default)]
    pub check: Option<Check>,
    #[serde(default)]
    pub dangerous: bool,
    #[serde(default)]
    pub allow_elevation: ElevationPolicy,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPolicy {
    #[default]
    None,
    GitCredential,
    Sudo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseProvider {
    LightBurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseMethod {
    VendorUi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateLicenseAction {
    pub provider: LicenseProvider,
    pub method: LicenseMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppBundleIdentity {
    pub bundle_identifier: String,
    pub team_identifier: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppStoreOperation {
    #[default]
    Install,
    Get,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppStoreInstallAction {
    pub app_id: u64,
    #[serde(default)]
    pub operation: AppStoreOperation,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseChannel {
    #[default]
    Release,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BambuStudioReleaseAction {
    #[serde(default)]
    pub channel: ReleaseChannel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveFormat {
    #[default]
    Auto,
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
}

fn default_archive_max_unpacked_bytes() -> u64 {
    10 * 1024 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    CreateDirectory(CreateDirectoryAction),
    InspectPath(InspectPathAction),
    GitClone {
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    BrewInstall {
        package: String,
        #[serde(default)]
        cask: bool,
    },
    RunCommand {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        shell: ShellMode,
    },
    RunScript {
        interpreter: ScriptInterpreter,
        /// Path to a script file. Inline script bodies are intentionally not
        /// accepted so plans and task packs do not become secret-bearing
        /// arbitrary shell payloads.
        script: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    DownloadFile {
        url: String,
        dest: String,
        checksum: Checksum,
    },
    ExtractArchive {
        src: String,
        dest: String,
        #[serde(default)]
        format: ArchiveFormat,
        #[serde(default = "default_archive_max_unpacked_bytes")]
        max_unpacked_bytes: u64,
    },
    InstallDmg {
        dmg: String,
        #[serde(default)]
        app_name: Option<String>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        identity: Option<AppBundleIdentity>,
    },
    InstallPkg {
        pkg: String,
        #[serde(default)]
        target: Option<String>,
    },
    MacosRequirements {
        minimum_version: String,
        #[serde(default)]
        require_rosetta_on_apple_silicon: bool,
    },
    AppStoreInstall(AppStoreInstallAction),
    BambuStudioRelease(BambuStudioReleaseAction),
    ActivateLicense(ActivateLicenseAction),
}

impl Task {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("task id must not be empty".into());
        }
        if self.id.contains('/') {
            return Err(format!("task {} id must not contain '/'", self.id));
        }
        if self.name.trim().is_empty() {
            return Err(format!("task {} name must not be empty", self.id));
        }
        if self.description.trim().is_empty() {
            return Err(format!("task {} description must not be empty", self.id));
        }
        if self.steps.is_empty() && self.scenarios.is_empty() {
            return Err(format!("task {} has no steps or scenarios", self.id));
        }
        if !self.steps.is_empty() && !self.scenarios.is_empty() {
            return Err(format!(
                "task {} must define either steps or scenarios, not both",
                self.id
            ));
        }

        let mut scenario_ids = std::collections::BTreeSet::new();
        for scenario_id in &self.scenarios {
            if scenario_id.trim().is_empty() {
                return Err(format!(
                    "task {} contains an empty scenario reference",
                    self.id
                ));
            }
            if scenario_id.contains('/') {
                return Err(format!(
                    "task {} scenario reference {} must not contain '/'",
                    self.id, scenario_id
                ));
            }
            if !scenario_ids.insert(scenario_id) {
                return Err(format!(
                    "task {} includes scenario {} more than once",
                    self.id, scenario_id
                ));
            }
        }

        let mut step_ids = std::collections::BTreeSet::new();
        for step in &self.steps {
            step.validate()?;
            if !step_ids.insert(&step.id) {
                return Err(format!(
                    "task {} contains duplicate step id {}",
                    self.id, step.id
                ));
            }
        }
        Ok(())
    }

    pub fn is_template(&self) -> bool {
        !self.scenarios.is_empty() || !self.resolved_scenarios.is_empty()
    }

    pub fn included_scenarios(&self) -> &[String] {
        if self.scenarios.is_empty() {
            &self.resolved_scenarios
        } else {
            &self.scenarios
        }
    }
}

impl Step {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("step id must not be empty".into());
        }
        match &self.action {
            Action::CreateDirectory(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("create-directory")?;
            }
            Action::InspectPath(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("inspect-path")?;
                if let Some(expectation) = &action.expect {
                    expectation.validate(&self.id)?;
                }
            }
            Action::GitClone { repo, dest, branch } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
                }
                if branch
                    .as_ref()
                    .is_some_and(|branch| branch.trim().is_empty())
                {
                    return Err(format!("step {} git branch must not be empty", self.id));
                }
            }
            Action::BrewInstall { package, .. } => {
                if package.trim().is_empty() {
                    return Err(format!("step {} requires package", self.id));
                }
            }
            Action::RunCommand { program, shell, .. } => {
                if program.trim().is_empty() {
                    return Err(format!("step {} requires program", self.id));
                }
                if matches!(shell, ShellMode::Allow) && !self.dangerous {
                    return Err(format!(
                        "step {} enables shell mode but is not marked dangerous",
                        self.id
                    ));
                }
            }
            Action::RunScript { script, cwd, .. } => {
                if script.trim().is_empty() {
                    return Err(format!("step {} requires script", self.id));
                }
                if script.contains(['\n', '\r']) {
                    return Err(format!(
                        "step {} script must be a file path, not inline source",
                        self.id
                    ));
                }
                if cwd
                    .as_deref()
                    .is_some_and(|directory| directory.trim().is_empty())
                {
                    return Err(format!("step {} script cwd must not be empty", self.id));
                }
                if !self.dangerous {
                    return Err(format!(
                        "step {} runs a script but is not marked dangerous",
                        self.id
                    ));
                }
            }
            Action::DownloadFile {
                url,
                dest,
                checksum,
            } => {
                if url.trim().is_empty()
                    || dest.trim().is_empty()
                    || checksum.sha256.trim().is_empty()
                {
                    return Err(format!(
                        "step {} requires url, dest, and checksum.sha256",
                        self.id
                    ));
                }
            }
            Action::ExtractArchive {
                src,
                dest,
                max_unpacked_bytes,
                ..
            } => {
                if src.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires src and dest", self.id));
                }
                if *max_unpacked_bytes == 0 {
                    return Err(format!(
                        "step {} max_unpacked_bytes must be greater than zero",
                        self.id
                    ));
                }
            }
            Action::InstallDmg {
                dmg,
                app_name,
                target,
                identity,
            } => {
                if dmg.trim().is_empty() {
                    return Err(format!("step {} requires dmg", self.id));
                }
                if let Some(app_name) = app_name {
                    let path = std::path::Path::new(app_name);
                    if app_name.trim().is_empty()
                        || path.components().count() != 1
                        || path.extension().and_then(|ext| ext.to_str()) != Some("app")
                    {
                        return Err(format!(
                            "step {} app_name must be a single .app bundle name",
                            self.id
                        ));
                    }
                }
                if target
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(format!("step {} target must not be empty", self.id));
                }
                if let Some(identity) = identity {
                    if app_name.is_none() {
                        return Err(format!(
                            "step {} requires app_name when identity is set",
                            self.id
                        ));
                    }
                    if !valid_bundle_identifier(&identity.bundle_identifier) {
                        return Err(format!(
                            "step {} identity.bundle_identifier is invalid",
                            self.id
                        ));
                    }
                    if identity.team_identifier.is_empty()
                        || !identity
                            .team_identifier
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric())
                    {
                        return Err(format!(
                            "step {} identity.team_identifier is invalid",
                            self.id
                        ));
                    }
                    if !valid_version(&identity.version) {
                        return Err(format!("step {} identity.version is invalid", self.id));
                    }
                }
            }
            Action::InstallPkg { pkg, .. } => {
                if pkg.trim().is_empty() {
                    return Err(format!("step {} requires pkg", self.id));
                }
            }
            Action::BambuStudioRelease(_) => {}
            Action::MacosRequirements {
                minimum_version, ..
            } => {
                if !valid_version(minimum_version) {
                    return Err(format!(
                        "step {} minimum_version must contain dot-separated integers",
                        self.id
                    ));
                }
            }
            Action::AppStoreInstall(action) => {
                if action.app_id == 0 {
                    return Err(format!("step {} app_id must be greater than zero", self.id));
                }
                if !matches!(self.auth, AuthPolicy::None)
                    || !matches!(self.allow_elevation, ElevationPolicy::Forbidden)
                {
                    return Err(format!(
                        "step {} app-store-install must not request authentication or elevation",
                        self.id
                    ));
                }
            }
            Action::ActivateLicense(_) => {}
        }
        Ok(())
    }

    fn validate_typed_filesystem_policy(&self, action: &str) -> Result<(), String> {
        if !matches!(self.auth, AuthPolicy::None)
            || !matches!(self.allow_elevation, ElevationPolicy::Forbidden)
        {
            return Err(format!(
                "step {} {} must not request authentication or elevation",
                self.id, action
            ));
        }
        if self.dangerous {
            return Err(format!(
                "step {} {} is typed and must not be marked dangerous",
                self.id, action
            ));
        }
        if self.check.is_some() {
            return Err(format!(
                "step {} {} must not use check; use the typed action's intrinsic idempotency or expect assertions",
                self.id, action
            ));
        }
        Ok(())
    }
}

fn valid_version(value: &str) -> bool {
    !value.trim().is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn valid_bundle_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
}
