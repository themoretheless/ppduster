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
