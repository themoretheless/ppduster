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
    #[serde(default)]
    pub sha256: Option<String>,
}

impl PathExpectation {
    fn has_metadata_assertion(&self) -> bool {
        self.kind.is_some()
            || self.empty.is_some()
            || self.min_size_bytes.is_some()
            || self.max_size_bytes.is_some()
            || self.modified_at_or_after.is_some()
            || self.modified_at_or_before.is_some()
            || self.sha256.is_some()
    }

    fn is_empty(&self) -> bool {
        self.exists.is_none() && !self.has_metadata_assertion()
    }

    fn validate(&self, step_id: &str) -> Result<(), String> {
        if self.is_empty() {
            return Err(format!(
                "step {} path expectation must contain at least one assertion",
                step_id
            ));
        }
        if matches!(self.exists, Some(false)) && self.has_metadata_assertion() {
            return Err(format!(
                "step {} path expectation exists: false cannot be combined with metadata assertions",
                step_id
            ));
        }
        if let (Some(minimum), Some(maximum)) = (self.min_size_bytes, self.max_size_bytes) {
            if minimum > maximum {
                return Err(format!(
                    "step {} path expectation min_size_bytes must not exceed max_size_bytes",
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
                    "step {} path expectation modified_at_or_after must not be later than modified_at_or_before",
                    step_id
                ));
            }
        }
        if matches!(self.empty, Some(true)) && self.min_size_bytes.is_some_and(|size| size > 0) {
            return Err(format!(
                "step {} path expectation empty: true cannot require a positive min_size_bytes",
                step_id
            ));
        }
        if let Some(sha256) = &self.sha256 {
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!(
                    "step {} path expectation sha256 must contain exactly 64 hexadecimal characters",
                    step_id
                ));
            }
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
    /// Compute SHA-256 for a regular file. Symlinks are never followed.
    #[serde(default)]
    pub sha256: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<PathExpectation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyPathAction {
    pub src: String,
    pub dest: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteConflictPolicy {
    #[default]
    Fail,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileAction {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub on_conflict: WriteConflictPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovePathAction {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NpmRegistryFileSpec {
    pub scope: String,
    pub registry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NugetRegistryFileSpec {
    pub public_source_name: String,
    pub public_source: String,
    pub source_name: String,
    pub source: String,
    pub package_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedSecretsSpec {
    pub profile: String,
    pub username_env: String,
    pub token_env: String,
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
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StepCondition {
    ExitCode {
        step: String,
        codes: Vec<u32>,
    },
    Path {
        path: String,
        expect: PathExpectation,
    },
    All {
        conditions: Vec<StepCondition>,
    },
    Any {
        conditions: Vec<StepCondition>,
    },
    Not {
        condition: Box<StepCondition>,
    },
}

impl StepCondition {
    fn validate(&self, step_id: &str) -> Result<(), String> {
        let mut nodes = 0usize;
        self.validate_inner(step_id, 1, &mut nodes)
    }

    fn validate_inner(&self, step_id: &str, depth: usize, nodes: &mut usize) -> Result<(), String> {
        const MAX_DEPTH: usize = 32;
        const MAX_NODES: usize = 256;

        if depth > MAX_DEPTH {
            return Err(format!(
                "step {} condition nesting exceeds maximum depth {}",
                step_id, MAX_DEPTH
            ));
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_NODES {
            return Err(format!(
                "step {} condition tree exceeds maximum size of {} nodes",
                step_id, MAX_NODES
            ));
        }

        match self {
            Self::ExitCode { step, codes } => {
                if step.trim().is_empty() {
                    return Err(format!(
                        "step {} exit-code condition requires a source step",
                        step_id
                    ));
                }
                if codes.is_empty() {
                    return Err(format!(
                        "step {} exit-code condition requires at least one code",
                        step_id
                    ));
                }
                let mut seen = std::collections::BTreeSet::new();
                for code in codes {
                    if !seen.insert(code) {
                        return Err(format!(
                            "step {} exit-code condition contains duplicate code {}",
                            step_id, code
                        ));
                    }
                }
            }
            Self::Path { path, expect } => {
                if path.trim().is_empty() {
                    return Err(format!("step {} path condition requires a path", step_id));
                }
                expect.validate(step_id)?;
            }
            Self::All { conditions } | Self::Any { conditions } => {
                if conditions.is_empty() {
                    return Err(format!(
                        "step {} {} condition requires at least one child condition",
                        step_id,
                        match self {
                            Self::All { .. } => "all",
                            Self::Any { .. } => "any",
                            _ => unreachable!(),
                        }
                    ));
                }
                for condition in conditions {
                    condition.validate_inner(step_id, depth + 1, nodes)?;
                }
            }
            Self::Not { condition } => {
                condition.validate_inner(step_id, depth + 1, nodes)?;
            }
        }
        Ok(())
    }

    fn try_for_each_exit_code<F>(&self, visit: &mut F) -> Result<(), String>
    where
        F: FnMut(&str, &[u32]) -> Result<(), String>,
    {
        match self {
            Self::ExitCode { step, codes } => visit(step, codes),
            Self::Path { .. } => Ok(()),
            Self::All { conditions } | Self::Any { conditions } => {
                for condition in conditions {
                    condition.try_for_each_exit_code(visit)?;
                }
                Ok(())
            }
            Self::Not { condition } => condition.try_for_each_exit_code(visit),
        }
    }

    fn prefix_source_step(&mut self, prefix: &str) {
        match self {
            Self::ExitCode { step, .. } => *step = format!("{prefix}/{step}"),
            Self::Path { .. } => {}
            Self::All { conditions } | Self::Any { conditions } => {
                for condition in conditions {
                    condition.prefix_source_step(prefix);
                }
            }
            Self::Not { condition } => condition.prefix_source_step(prefix),
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<StepCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<StepCondition>,
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

fn default_script_success_exit_codes() -> Vec<u32> {
    vec![0]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    CreateDirectory(CreateDirectoryAction),
    InspectPath(InspectPathAction),
    CopyPath(CopyPathAction),
    WriteFile(WriteFileAction),
    RemovePath(RemovePathAction),
    GitClone {
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    GitInspect {
        repo: String,
        dest: String,
    },
    GitCloneIfMissing {
        repo: String,
        dest: String,
        #[serde(default)]
        branch: Option<String>,
    },
    GitFetch {
        repo: String,
        dest: String,
        branch: String,
    },
    GitFastForward {
        repo: String,
        dest: String,
        branch: String,
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
        #[serde(default = "default_script_success_exit_codes")]
        success_exit_codes: Vec<u32>,
    },
    ConfigurePackageRegistryFiles {
        secrets: EncryptedSecretsSpec,
        npm: NpmRegistryFileSpec,
        nuget: NugetRegistryFileSpec,
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
        self.validate_metadata()?;
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

        self.validate_steps()
    }

    pub(crate) fn validate_executable(&self) -> Result<(), String> {
        self.validate_metadata()?;
        if self.steps.is_empty() {
            return Err(format!("task {} has no executable steps", self.id));
        }
        self.validate_steps()
    }

    fn validate_metadata(&self) -> Result<(), String> {
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
        Ok(())
    }

    fn validate_steps(&self) -> Result<(), String> {
        let mut step_ids = std::collections::BTreeSet::<&str>::new();
        let mut script_exit_codes = std::collections::BTreeMap::<&str, &[u32]>::new();
        for step in &self.steps {
            step.validate()?;
            if step_ids.contains(step.id.as_str()) {
                return Err(format!(
                    "task {} contains duplicate step id {}",
                    self.id, step.id
                ));
            }
            for condition in [step.when.as_ref(), step.require.as_ref()]
                .into_iter()
                .flatten()
            {
                condition.try_for_each_exit_code(&mut |source_id, codes| {
                    if !step_ids.contains(source_id) {
                        return Err(format!(
                            "step {} exit-code condition must reference an earlier step, got {}",
                            step.id, source_id
                        ));
                    }
                    let Some(success_codes) = script_exit_codes.get(source_id) else {
                        return Err(format!(
                            "step {} exit-code condition source {} is not a run-script step",
                            step.id, source_id
                        ));
                    };
                    for code in codes {
                        if !success_codes.contains(code) {
                            return Err(format!(
                                "step {} condition code {} is not listed in source step {} success_exit_codes",
                                step.id, code, source_id
                            ));
                        }
                    }
                    Ok(())
                })?;
            }
            step_ids.insert(step.id.as_str());
            if let Action::RunScript {
                success_exit_codes, ..
            } = &step.action
            {
                script_exit_codes.insert(step.id.as_str(), success_exit_codes);
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
    pub(crate) fn prefix_condition_step(&mut self, prefix: &str) {
        if let Some(condition) = &mut self.when {
            condition.prefix_source_step(prefix);
        }
        if let Some(condition) = &mut self.require {
            condition.prefix_source_step(prefix);
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("step id must not be empty".into());
        }
        if let Some(condition) = &self.when {
            condition.validate(&self.id)?;
        }
        if let Some(condition) = &self.require {
            condition.validate(&self.id)?;
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
            Action::CopyPath(action) => {
                if action.src.trim().is_empty() || action.dest.trim().is_empty() {
                    return Err(format!("step {} requires src and dest", self.id));
                }
                if action.src == action.dest {
                    return Err(format!(
                        "step {} copy-path src and dest must be different",
                        self.id
                    ));
                }
                self.validate_typed_filesystem_policy("copy-path")?;
            }
            Action::WriteFile(action) => {
                const MAX_CONTENT_BYTES: usize = 1024 * 1024;

                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                if action.content.len() > MAX_CONTENT_BYTES {
                    return Err(format!(
                        "step {} write-file content must not exceed {} bytes",
                        self.id, MAX_CONTENT_BYTES
                    ));
                }
                if action.content.contains('\0') {
                    return Err(format!(
                        "step {} write-file content must not contain NUL bytes",
                        self.id
                    ));
                }
                self.validate_typed_filesystem_policy("write-file")?;
            }
            Action::RemovePath(action) => {
                if action.path.trim().is_empty() {
                    return Err(format!("step {} requires path", self.id));
                }
                self.validate_typed_filesystem_policy("remove-path")?;
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
            Action::GitInspect { repo, dest } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
                }
            }
            Action::GitCloneIfMissing { repo, dest, branch } => {
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
            Action::GitFetch { repo, dest, branch }
            | Action::GitFastForward { repo, dest, branch } => {
                if repo.trim().is_empty() || dest.trim().is_empty() || branch.trim().is_empty() {
                    return Err(format!("step {} requires repo, dest, and branch", self.id));
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
            Action::RunScript {
                script,
                cwd,
                success_exit_codes,
                ..
            } => {
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
                if success_exit_codes.is_empty() {
                    return Err(format!(
                        "step {} success_exit_codes must contain at least one exit code",
                        self.id
                    ));
                }
                let mut seen_exit_codes = std::collections::BTreeSet::new();
                for code in success_exit_codes {
                    if !seen_exit_codes.insert(code) {
                        return Err(format!(
                            "step {} success_exit_codes contains duplicate exit code {}",
                            self.id, code
                        ));
                    }
                }
            }
            Action::ConfigurePackageRegistryFiles {
                secrets,
                npm,
                nuget,
            } => {
                validate_secret_profile(&secrets.profile, &self.id, "secrets.profile")?;
                validate_env_name(&secrets.username_env, &self.id, "secrets.username_env")?;
                validate_env_name(&secrets.token_env, &self.id, "secrets.token_env")?;
                if secrets.username_env == secrets.token_env
                    || is_reserved_secret_env(&secrets.username_env)
                    || is_reserved_secret_env(&secrets.token_env)
                {
                    return Err(format!(
                        "step {} requires distinct, non-reserved secret environment names",
                        self.id
                    ));
                }
                validate_https_url(&npm.registry, &self.id, "npm.registry")?;
                validate_https_url(&nuget.public_source, &self.id, "nuget.public_source")?;
                validate_https_url(&nuget.source, &self.id, "nuget.source")?;
                if urls_equal(&nuget.public_source, &nuget.source) {
                    return Err(format!(
                        "step {} requires distinct public and private NuGet sources",
                        self.id
                    ));
                }
                if !is_valid_npm_scope(&npm.scope) {
                    return Err(format!(
                        "step {} requires npm.scope in @namespace form",
                        self.id
                    ));
                }
                validate_xml_name(
                    &nuget.public_source_name,
                    &self.id,
                    "nuget.public_source_name",
                )?;
                validate_xml_name(&nuget.source_name, &self.id, "nuget.source_name")?;
                if nuget
                    .public_source_name
                    .eq_ignore_ascii_case(&nuget.source_name)
                {
                    return Err(format!(
                        "step {} requires distinct NuGet source names",
                        self.id
                    ));
                }
                if nuget.package_patterns.is_empty()
                    || nuget
                        .package_patterns
                        .iter()
                        .any(|pattern| !is_valid_nuget_package_pattern(pattern))
                {
                    return Err(format!(
                        "step {} requires specific NuGet package prefix patterns",
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

fn validate_https_url(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let trimmed = value.trim();
    let Some(remainder) = trimmed.strip_prefix("https://") else {
        return Err(format!(
            "step {step_id} requires {field} to be an HTTPS URL"
        ));
    };
    let (authority, path) = remainder
        .split_once('/')
        .map_or((remainder, ""), |(authority, path)| (authority, path));
    if trimmed != value
        || !is_valid_url_authority(authority)
        || !is_valid_url_path(path)
        || remainder.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '$' | '{' | '}' | '%'))
    {
        return Err(format!(
            "step {step_id} requires {field} to be an HTTPS URL"
        ));
    }
    Ok(())
}

fn is_valid_url_authority(value: &str) -> bool {
    if value.is_empty() || value.contains('@') {
        return false;
    }
    if let Some(ipv6) = value.strip_prefix('[') {
        let Some((address, port)) = ipv6.split_once(']') else {
            return false;
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok() && is_valid_port_suffix(port);
    }
    if value.matches(':').count() > 1 {
        return false;
    }
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    is_valid_dns_host(host) && port.map(is_valid_port).unwrap_or(true)
}

fn is_valid_port_suffix(value: &str) -> bool {
    value.is_empty() || value.strip_prefix(':').map(is_valid_port).unwrap_or(false)
}

fn is_valid_port(value: &str) -> bool {
    value.parse::<u16>().map(|port| port != 0).unwrap_or(false)
}

fn is_valid_dns_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && label
                .chars()
                .next()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false)
            && label
                .chars()
                .last()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false)
    })
}

fn is_valid_url_path(value: &str) -> bool {
    value.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '-'
                    | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | ':'
                    | '@'
            )
    })
}

fn urls_equal(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

fn validate_xml_name(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(format!("step {step_id} requires {field}"));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "step {step_id} requires {field} to be a portable XML name"
        ));
    }
    Ok(())
}

fn is_valid_nuget_package_pattern(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let star_count = trimmed.chars().filter(|c| *c == '*').count();
    (star_count == 0 || (star_count == 1 && trimmed.ends_with('*')))
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*'))
}

fn is_valid_npm_scope(value: &str) -> bool {
    let Some(namespace) = value.strip_prefix('@') else {
        return false;
    };
    !namespace.is_empty()
        && namespace
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn validate_secret_profile(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let valid = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.len() <= 64
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });
    if !valid {
        return Err(format!(
            "step {step_id} requires {field} to match [a-z0-9][a-z0-9._-]{{0,63}}"
        ));
    }
    Ok(())
}

fn validate_env_name(value: &str, step_id: &str, field: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(format!("step {step_id} requires {field}"));
    };
    if !(first.is_ascii_uppercase() || first == '_')
        || !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "step {step_id} requires {field} to be an uppercase environment variable name"
        ));
    }
    Ok(())
}

fn is_reserved_secret_env(value: &str) -> bool {
    matches!(
        value,
        "PATH"
            | "HOME"
            | "USERPROFILE"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "PPDUSTER_AUDIT_LOG"
    ) || value.starts_with("NPM_CONFIG_")
        || value.starts_with("DOTNET_")
        || value.starts_with("MSBUILD")
        || value.starts_with("DYLD_")
        || value.starts_with("NUGET_")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn package_registry_step() -> Step {
        Step {
            id: "package-config".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::ConfigurePackageRegistryFiles {
                secrets: EncryptedSecretsSpec {
                    profile: "github-packages".into(),
                    username_env: "GITHUB_PACKAGES_USER".into(),
                    token_env: "GITHUB_PACKAGES_TOKEN".into(),
                },
                npm: NpmRegistryFileSpec {
                    scope: "@dodopizza".into(),
                    registry: "https://npm.pkg.github.com/".into(),
                },
                nuget: NugetRegistryFileSpec {
                    public_source_name: "nuget.org".into(),
                    public_source: "https://api.nuget.org/v3/index.json".into(),
                    source_name: "github".into(),
                    source: "https://nuget.pkg.github.com/dodopizza/index.json".into(),
                    package_patterns: vec!["Dodo.*".into()],
                },
            },
        }
    }

    #[test]
    fn package_registry_action_validates() {
        package_registry_step().validate().unwrap();
    }

    #[test]
    fn package_registry_action_rejects_broad_nuget_mapping() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { nuget, .. } = &mut step.action else {
            unreachable!()
        };
        nuget.package_patterns = vec!["*".into()];

        let err = step.validate().unwrap_err();
        assert!(err.contains("specific NuGet package prefix patterns"));
    }

    #[test]
    fn package_registry_action_rejects_secret_bearing_or_insecure_urls() {
        for invalid in [
            "http://npm.pkg.github.com/",
            "https://token@npm.pkg.github.com/",
            "https://npm.pkg.github.com/?token=secret",
            "https://npm.pkg.github.com/#secret",
            "https://:443/",
            "https://[]/",
            "https://npm.pkg.github.com/${AWS_SECRET_ACCESS_KEY}/",
            "https://npm.pkg.github.com/%AWS_SECRET_ACCESS_KEY%/",
            "https://npm.pkg.github.com/\u{0000}/",
        ] {
            let mut step = package_registry_step();
            let Action::ConfigurePackageRegistryFiles { npm, .. } = &mut step.action else {
                unreachable!()
            };
            npm.registry = invalid.into();
            assert!(step.validate().is_err(), "accepted invalid URL: {invalid}");
        }
    }

    #[test]
    fn package_registry_action_rejects_nonportable_env_names() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
            unreachable!()
        };
        secrets.token_env = "github-token".into();

        let err = step.validate().unwrap_err();
        assert!(err.contains("uppercase environment variable name"));
    }

    #[test]
    fn package_registry_action_rejects_reserved_secret_env_names() {
        let mut step = package_registry_step();
        let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
            unreachable!()
        };
        secrets.token_env = "PATH".into();

        let err = step.validate().unwrap_err();
        assert!(err.contains("non-reserved secret environment names"));
    }

    #[test]
    fn package_registry_action_rejects_invalid_secret_profiles() {
        for invalid in [
            "",
            "GitHub-packages",
            "-github-packages",
            "github packages",
            "github/packages",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let mut step = package_registry_step();
            let Action::ConfigurePackageRegistryFiles { secrets, .. } = &mut step.action else {
                unreachable!()
            };
            secrets.profile = invalid.into();

            let err = step.validate().unwrap_err();
            assert!(err.contains("[a-z0-9][a-z0-9._-]{0,63}"));
        }
    }
}
