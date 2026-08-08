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
    },
    InstallPkg {
        pkg: String,
        #[serde(default)]
        target: Option<String>,
    },
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
            Action::InstallDmg { dmg, .. } => {
                if dmg.trim().is_empty() {
                    return Err(format!("step {} requires dmg", self.id));
                }
            }
            Action::InstallPkg { pkg, .. } => {
                if pkg.trim().is_empty() {
                    return Err(format!("step {} requires pkg", self.id));
                }
            }
        }
        Ok(())
    }
}
