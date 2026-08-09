use crate::rules::Platform;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustRequirement {
    BundledOnly,
    UserConfigAllowed,
    ExternalAllowed,
}

impl Default for TrustRequirement {
    fn default() -> Self {
        Self::BundledOnly
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShellMode {
    Forbidden,
    Allow,
}

impl Default for ShellMode {
    fn default() -> Self {
        Self::Forbidden
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElevationPolicy {
    Forbidden,
    Allow,
}

impl Default for ElevationPolicy {
    fn default() -> Self {
        Self::Forbidden
    }
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
    #[serde(default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPolicy {
    None,
    GitCredential,
    Sudo,
}

impl Default for AuthPolicy {
    fn default() -> Self {
        Self::None
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
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
    DownloadFile {
        url: String,
        dest: String,
        checksum: Checksum,
    },
    ExtractArchive {
        src: String,
        dest: String,
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
        if self.steps.is_empty() {
            return Err(format!("task {} has no steps", self.id));
        }
        for step in &self.steps {
            step.validate()?;
        }
        Ok(())
    }
}

impl Step {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("step id must not be empty".into());
        }
        match &self.action {
            Action::GitClone { repo, dest, .. } => {
                if repo.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires repo and dest", self.id));
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
            Action::ExtractArchive { src, dest } => {
                if src.trim().is_empty() || dest.trim().is_empty() {
                    return Err(format!("step {} requires src and dest", self.id));
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
                if !matches!(self.auth, AuthPolicy::Sudo)
                    || !matches!(self.allow_elevation, ElevationPolicy::Allow)
                {
                    return Err(format!(
                        "step {} app-store-install requires auth: sudo plus allow_elevation: allow",
                        self.id
                    ));
                }
            }
            Action::ActivateLicense(_) => {}
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
