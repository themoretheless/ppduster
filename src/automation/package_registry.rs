use crate::automation::task::{EncryptedSecretsSpec, NpmRegistryFileSpec, NugetRegistryFileSpec};
use anyhow::{anyhow, bail, Context, Result};
use same_file::Handle;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
enum ConfigFileState {
    Missing,
    Exact,
    Conflict(String),
}

impl ConfigFileState {
    fn summary(&self) -> String {
        match self {
            Self::Missing => "add".into(),
            Self::Exact => "noop (exact match)".into(),
            Self::Conflict(reason) => format!("conflict ({reason})"),
        }
    }
}

struct RenderedPackageRegistryFiles {
    npmrc: Vec<u8>,
    nuget_config: Vec<u8>,
}

struct StagedConfigFile {
    temp: tempfile::NamedTempFile,
    target: PathBuf,
    identity: Handle,
}

pub(super) fn plan_summary(
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
) -> Result<String> {
    let root = package_registry_root()?;
    let rendered = render_package_registry_files(secrets, npm, nuget)?;
    let npmrc_path = root.join(".npmrc");
    let nuget_path = root.join("NuGet.Config");
    let npmrc_state = config_file_state_with_case_guard(&npmrc_path, &rendered.npmrc)?;
    let nuget_state = config_file_state_with_case_guard(&nuget_path, &rendered.nuget_config)?;

    Ok(format!(
        "configure project package registries in {}: {} [{}; expected sha256 {}], {} [{}; expected sha256 {}]; npm scope {} -> {}; NuGet <clear/> replaces inherited sources with {} -> {} and {} -> {}, mapping private patterns {:?}; encrypted secrets profile {} supplies runtime references ${{{}}}, %{}%, and %{}%",
        root.display(),
        npmrc_path.display(),
        npmrc_state.summary(),
        sha256_bytes(&rendered.npmrc),
        nuget_path.display(),
        nuget_state.summary(),
        sha256_bytes(&rendered.nuget_config),
        npm.scope,
        npm.registry,
        nuget.public_source_name,
        nuget.public_source,
        nuget.source_name,
        nuget.source,
        nuget.package_patterns,
        secrets.profile,
        secrets.token_env,
        secrets.username_env,
        secrets.token_env,
    ))
}

pub(super) fn apply(
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
) -> Result<String> {
    let root = package_registry_root()?;
    validate_package_workspace(&root)?;
    apply_at(&root, secrets, npm, nuget)
}

pub(super) fn is_satisfied(
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
) -> Result<Option<String>> {
    let root = package_registry_root()?;
    validate_package_workspace(&root)?;
    let rendered = render_package_registry_files(secrets, npm, nuget)?;
    let npmrc_exact = matches!(
        config_file_state_with_case_guard(&root.join(".npmrc"), &rendered.npmrc)?,
        ConfigFileState::Exact
    );
    let nuget_exact = matches!(
        config_file_state_with_case_guard(&root.join("NuGet.Config"), &rendered.nuget_config)?,
        ConfigFileState::Exact
    );
    Ok((npmrc_exact && nuget_exact).then(|| {
        format!(
            "project package registry files already match in {}",
            root.display()
        )
    }))
}

fn apply_at(
    root: &Path,
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
) -> Result<String> {
    apply_at_with_commit_hook(root, secrets, npm, nuget, |_, _| Ok(()))
}

fn apply_at_with_commit_hook<F>(
    root: &Path,
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
    mut after_commit: F,
) -> Result<String>
where
    F: FnMut(usize, &Path) -> Result<()>,
{
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize package workspace {}", root.display()))?;
    let rendered = render_package_registry_files(secrets, npm, nuget)?;
    let targets = [
        (root.join(".npmrc"), rendered.npmrc.as_slice()),
        (root.join("NuGet.Config"), rendered.nuget_config.as_slice()),
    ];

    let mut missing = Vec::new();
    for (path, expected) in &targets {
        match config_file_state_with_case_guard(path, expected)? {
            ConfigFileState::Missing => missing.push((path.clone(), *expected)),
            ConfigFileState::Exact => {}
            ConfigFileState::Conflict(reason) => {
                bail!(
                    "refusing to replace existing package registry file {}: {}",
                    path.display(),
                    reason
                );
            }
        }
    }

    if missing.is_empty() {
        return Ok(format!(
            "package registry files already match in {}",
            root.display()
        ));
    }

    let mut staged = Vec::new();
    for (target, bytes) in &missing {
        staged.push(stage_config_file(target, bytes)?);
    }

    let mut created = 0usize;
    for (index, staged_file) in staged.iter().enumerate() {
        if fs::symlink_metadata(&staged_file.target).is_ok() {
            bail!(
                "package registry target appeared during apply: {}",
                staged_file.target.display()
            );
        }
        if let Err(err) = fs::hard_link(staged_file.temp.path(), &staged_file.target) {
            return Err(err).with_context(|| {
                format!(
                    "atomically install package registry file {}",
                    staged_file.target.display()
                )
            });
        }
        created += 1;
        let installed_identity = match Handle::from_path(&staged_file.target) {
            Ok(identity) => identity,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "verify installed package registry file identity {}",
                        staged_file.target.display()
                    )
                });
            }
        };
        if installed_identity != staged_file.identity {
            bail!(
                "package registry staging file changed during apply: {}",
                staged_file.temp.path().display()
            );
        }
        if let Err(err) = after_commit(index, &staged_file.target) {
            return Err(err).context("run package registry commit hook");
        }
    }

    for (target, expected) in &targets {
        let actual = match fs::read(target) {
            Ok(actual) => actual,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("verify package registry file {}", target.display()));
            }
        };
        if !has_equivalent_line_endings(&actual, expected) {
            bail!(
                "package registry file changed during verification: {}",
                target.display()
            );
        }
    }

    Ok(format!(
        "created {} package registry file(s) in {}",
        created,
        root.display()
    ))
}

fn package_registry_root() -> Result<PathBuf> {
    let root = std::env::current_dir()
        .context("resolve package workspace")?
        .canonicalize()
        .context("canonicalize package workspace")?;
    if root.parent().is_none() {
        bail!("refusing to configure package registries at a filesystem root");
    }
    if let Some(home) = dirs::home_dir().and_then(|path| path.canonicalize().ok()) {
        if root == home {
            bail!("refusing to configure package registries in the home directory");
        }
    }
    Ok(root)
}

fn validate_package_workspace(root: &Path) -> Result<()> {
    if !root.join(".git").exists() {
        bail!(
            "package registry task must run from a Git repository root: {}",
            root.display()
        );
    }
    if !root.join("package.json").is_file() {
        bail!(
            "package registry task requires package.json in {}",
            root.display()
        );
    }
    let has_dotnet_marker = fs::read_dir(root)
        .with_context(|| format!("inspect package workspace {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            let path = entry.path();
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| {
                        ["sln", "slnx", "csproj", "fsproj", "vbproj"]
                            .iter()
                            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                    })
                    .unwrap_or(false)
        });
    if !has_dotnet_marker {
        bail!(
            "package registry task requires a .NET solution or project in {}",
            root.display()
        );
    }
    Ok(())
}

fn render_package_registry_files(
    secrets: &EncryptedSecretsSpec,
    npm: &NpmRegistryFileSpec,
    nuget: &NugetRegistryFileSpec,
) -> Result<RenderedPackageRegistryFiles> {
    let npm_registry = with_trailing_slash(&npm.registry);
    let npm_registry_without_scheme = npm_registry
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("npm registry must use HTTPS"))?;
    let npm_auth_key = format!("//{}:_authToken", npm_registry_without_scheme);
    let npmrc = format!(
        "{}:registry={}\n{}=${{{}}}\n",
        npm.scope, npm_registry, npm_auth_key, secrets.token_env
    );

    let mut private_patterns = String::new();
    for pattern in &nuget.package_patterns {
        private_patterns.push_str(&format!(
            "      <package pattern=\"{}\" />\n",
            escape_xml(pattern)
        ));
    }
    let nuget_config = format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
            "<configuration>\n",
            "  <packageSources>\n",
            "    <clear />\n",
            "    <add key=\"{}\" value=\"{}\" protocolVersion=\"3\" />\n",
            "    <add key=\"{}\" value=\"{}\" protocolVersion=\"3\" />\n",
            "  </packageSources>\n",
            "  <packageSourceMapping>\n",
            "    <clear />\n",
            "    <packageSource key=\"{}\">\n",
            "      <package pattern=\"*\" />\n",
            "    </packageSource>\n",
            "    <packageSource key=\"{}\">\n",
            "{}",
            "    </packageSource>\n",
            "  </packageSourceMapping>\n",
            "  <packageSourceCredentials>\n",
            "    <{}>\n",
            "      <add key=\"Username\" value=\"%{}%\" />\n",
            "      <add key=\"ClearTextPassword\" value=\"%{}%\" />\n",
            "    </{}>\n",
            "  </packageSourceCredentials>\n",
            "</configuration>\n"
        ),
        escape_xml(&nuget.public_source_name),
        escape_xml(&nuget.public_source),
        escape_xml(&nuget.source_name),
        escape_xml(&nuget.source),
        escape_xml(&nuget.public_source_name),
        escape_xml(&nuget.source_name),
        private_patterns,
        nuget.source_name,
        secrets.username_env,
        secrets.token_env,
        nuget.source_name,
    );

    Ok(RenderedPackageRegistryFiles {
        npmrc: npmrc.into_bytes(),
        nuget_config: nuget_config.into_bytes(),
    })
}

fn with_trailing_slash(value: &str) -> String {
    format!("{}/", value.trim_end_matches('/'))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn config_file_state(path: &Path, expected: &[u8]) -> Result<ConfigFileState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigFileState::Missing)
        }
        Err(err) => {
            return Err(err).with_context(|| format!("inspect config file {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(ConfigFileState::Conflict(
            "target is not a regular file".into(),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(ConfigFileState::Conflict(
                "target is a Windows reparse point".into(),
            ));
        }
    }
    #[cfg(unix)]
    let has_multiple_hard_links = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o022 != 0 {
            return Ok(ConfigFileState::Conflict(
                "target is writable by group or others".into(),
            ));
        }
        metadata.nlink() > 1
    };
    let actual = fs::read(path).with_context(|| format!("read config file {}", path.display()))?;
    if has_equivalent_line_endings(&actual, expected) {
        Ok(ConfigFileState::Exact)
    } else {
        #[cfg(unix)]
        if has_multiple_hard_links {
            return Ok(ConfigFileState::Conflict(
                "differing target has multiple hard links".into(),
            ));
        }
        Ok(ConfigFileState::Conflict(format!(
            "existing sha256 {}",
            sha256_bytes(&actual)
        )))
    }
}

fn has_equivalent_line_endings(actual: &[u8], expected: &[u8]) -> bool {
    if actual == expected {
        return true;
    }
    let mut normalized = Vec::with_capacity(actual.len());
    let mut index = 0;
    while index < actual.len() {
        if actual[index] == b'\r' {
            if actual.get(index + 1) != Some(&b'\n') {
                return false;
            }
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(actual[index]);
            index += 1;
        }
    }
    normalized == expected
}

fn config_file_state_with_case_guard(path: &Path, expected: &[u8]) -> Result<ConfigFileState> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config target has no parent: {}", path.display()))?;
    let expected_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("config target has no UTF-8 filename: {}", path.display()))?;
    for entry in fs::read_dir(parent)
        .with_context(|| format!("inspect config directory {}", parent.display()))?
    {
        let entry =
            entry.with_context(|| format!("inspect config directory {}", parent.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != expected_name && name.eq_ignore_ascii_case(expected_name) {
            return Ok(ConfigFileState::Conflict(format!(
                "case-variant target already exists: {}",
                entry.path().display()
            )));
        }
    }
    config_file_state(path, expected)
}

fn stage_config_file(target: &Path, contents: &[u8]) -> Result<StagedConfigFile> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("config target has no parent: {}", target.display()))?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("config target has no UTF-8 filename: {}", target.display()))?;
    let prefix = format!(".{file_name}.ppduster.");
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("create staged config for {}", target.display()))?;
    temp.as_file_mut()
        .write_all(contents)
        .with_context(|| format!("write staged config {}", temp.path().display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync staged config {}", temp.path().display()))?;
    let identity = Handle::from_file(
        temp.as_file()
            .try_clone()
            .with_context(|| format!("clone staged config handle {}", temp.path().display()))?,
    )
    .with_context(|| format!("identify staged config {}", temp.path().display()))?;
    Ok(StagedConfigFile {
        temp,
        target: target.to_path_buf(),
        identity,
    })
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_registry_specs() -> (
        EncryptedSecretsSpec,
        NpmRegistryFileSpec,
        NugetRegistryFileSpec,
    ) {
        (
            EncryptedSecretsSpec {
                profile: "github-packages".into(),
                username_env: "GITHUB_PACKAGES_USER".into(),
                token_env: "GITHUB_PACKAGES_TOKEN".into(),
            },
            NpmRegistryFileSpec {
                scope: "@dodopizza".into(),
                registry: "https://npm.pkg.github.com/".into(),
            },
            NugetRegistryFileSpec {
                public_source_name: "nuget.org".into(),
                public_source: "https://api.nuget.org/v3/index.json".into(),
                source_name: "github".into(),
                source: "https://nuget.pkg.github.com/dodopizza/index.json".into(),
                package_patterns: vec!["Dodo.*".into()],
            },
        )
    }

    #[test]
    fn renders_scoped_literal_credentials_and_mapping() {
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();
        let npmrc = String::from_utf8(rendered.npmrc).unwrap();
        let nuget = String::from_utf8(rendered.nuget_config).unwrap();

        assert_eq!(
            npmrc,
            "@dodopizza:registry=https://npm.pkg.github.com/\n\
//npm.pkg.github.com/:_authToken=${GITHUB_PACKAGES_TOKEN}\n"
        );
        assert_eq!(
            nuget,
            concat!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
                "<configuration>\n",
                "  <packageSources>\n",
                "    <clear />\n",
                "    <add key=\"nuget.org\" value=\"https://api.nuget.org/v3/index.json\" protocolVersion=\"3\" />\n",
                "    <add key=\"github\" value=\"https://nuget.pkg.github.com/dodopizza/index.json\" protocolVersion=\"3\" />\n",
                "  </packageSources>\n",
                "  <packageSourceMapping>\n",
                "    <clear />\n",
                "    <packageSource key=\"nuget.org\">\n",
                "      <package pattern=\"*\" />\n",
                "    </packageSource>\n",
                "    <packageSource key=\"github\">\n",
                "      <package pattern=\"Dodo.*\" />\n",
                "    </packageSource>\n",
                "  </packageSourceMapping>\n",
                "  <packageSourceCredentials>\n",
                "    <github>\n",
                "      <add key=\"Username\" value=\"%GITHUB_PACKAGES_USER%\" />\n",
                "      <add key=\"ClearTextPassword\" value=\"%GITHUB_PACKAGES_TOKEN%\" />\n",
                "    </github>\n",
                "  </packageSourceCredentials>\n",
                "</configuration>\n"
            )
        );
    }

    #[test]
    fn exact_npmrc_and_missing_nuget_config_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();
        let existing_npmrc = String::from_utf8(rendered.npmrc.clone())
            .unwrap()
            .replace('\n', "\r\n");
        fs::write(temp.path().join(".npmrc"), &existing_npmrc).unwrap();

        let result = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();

        assert!(result.contains("created 1"));
        assert_eq!(
            fs::read_to_string(temp.path().join(".npmrc")).unwrap(),
            existing_npmrc
        );
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            rendered.nuget_config
        );
    }

    #[test]
    fn apply_is_create_only_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();

        let first = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();
        assert!(first.contains("created 2"));
        let npmrc_before = fs::read(temp.path().join(".npmrc")).unwrap();
        let nuget_before = fs::read(temp.path().join("NuGet.Config")).unwrap();
        let second = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();
        assert!(second.contains("already match"));
        assert_eq!(fs::read(temp.path().join(".npmrc")).unwrap(), npmrc_before);
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            nuget_before
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in [".npmrc", "NuGet.Config"] {
                assert_eq!(
                    fs::metadata(temp.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn crlf_checkout_is_an_unchanged_semantic_match() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();
        let npmrc = String::from_utf8(rendered.npmrc)
            .unwrap()
            .replace('\n', "\r\n");
        let nuget_config = String::from_utf8(rendered.nuget_config)
            .unwrap()
            .replace('\n', "\r\n");
        fs::write(temp.path().join(".npmrc"), &npmrc).unwrap();
        fs::write(temp.path().join("NuGet.Config"), &nuget_config).unwrap();

        let result = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();

        assert!(result.contains("already match"));
        assert_eq!(
            fs::read_to_string(temp.path().join(".npmrc")).unwrap(),
            npmrc
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("NuGet.Config")).unwrap(),
            nuget_config
        );
    }

    #[test]
    fn conflict_writes_neither_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(".npmrc"),
            b"registry=https://example.test/\n",
        )
        .unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let err = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap_err();
        assert!(err.to_string().contains("refusing to replace"));
        assert_eq!(
            fs::read(temp.path().join(".npmrc")).unwrap(),
            b"registry=https://example.test/\n"
        );
        assert!(!temp.path().join("NuGet.Config").exists());
    }

    #[test]
    fn nuget_conflict_blocks_npmrc_creation() {
        let temp = tempfile::tempdir().unwrap();
        let existing = b"<configuration><!-- keep me --></configuration>\n";
        fs::write(temp.path().join("NuGet.Config"), existing).unwrap();
        let (secrets, npm, nuget) = package_registry_specs();

        let err = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap_err();

        assert!(err.to_string().contains("refusing to replace"));
        assert!(!temp.path().join(".npmrc").exists());
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            existing
        );
    }

    #[test]
    fn concurrent_second_target_never_clobbers_and_leaves_exact_partial() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();
        let concurrent = b"<configuration><!-- concurrent --></configuration>\n";

        let err = apply_at_with_commit_hook(temp.path(), &secrets, &npm, &nuget, |index, _| {
            if index == 0 {
                fs::write(temp.path().join("NuGet.Config"), concurrent)?;
            }
            Ok(())
        })
        .unwrap_err();

        assert!(err.to_string().contains("target appeared"));
        assert_eq!(
            fs::read(temp.path().join(".npmrc")).unwrap(),
            rendered.npmrc
        );
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            concurrent
        );
    }

    #[test]
    fn failure_never_deletes_a_concurrent_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let replacement = b"user replacement\n";
        let displaced = temp.path().join("displaced-npmrc");

        let err =
            apply_at_with_commit_hook(temp.path(), &secrets, &npm, &nuget, |index, target| {
                if index == 0 {
                    fs::rename(target, &displaced)?;
                    fs::write(target, replacement)?;
                    bail!("injected failure after replacement");
                }
                Ok(())
            })
            .unwrap_err();

        assert!(format!("{err:#}").contains("injected failure"));
        assert_eq!(fs::read(temp.path().join(".npmrc")).unwrap(), replacement);
        assert!(!temp.path().join("NuGet.Config").exists());
        assert!(displaced.exists());
    }

    #[test]
    fn failure_after_first_commit_leaves_a_recoverable_exact_partial() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();

        let err = apply_at_with_commit_hook(temp.path(), &secrets, &npm, &nuget, |index, _| {
            if index == 0 {
                bail!("injected failure after first commit");
            }
            Ok(())
        })
        .unwrap_err();

        assert!(format!("{err:#}").contains("injected failure"));
        assert_eq!(
            fs::read(temp.path().join(".npmrc")).unwrap(),
            rendered.npmrc
        );
        assert!(!temp.path().join("NuGet.Config").exists());

        let recovery = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();
        assert!(recovery.contains("created 1"));
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            rendered.nuget_config
        );
    }

    #[test]
    fn case_variant_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("nuget.config"), b"<configuration />\n").unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let err = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap_err();
        assert!(err.to_string().contains("case-variant target"));
        assert!(!temp.path().join(".npmrc").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_a_conflict() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"do not replace\n").unwrap();
        symlink(&outside, temp.path().join(".npmrc")).unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let err = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
        assert_eq!(fs::read(&outside).unwrap(), b"do not replace\n");
        assert!(!temp.path().join("NuGet.Config").exists());
    }

    #[cfg(unix)]
    #[test]
    fn differing_hard_link_is_a_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let outside = temp.path().join("shared-npmrc");
        fs::write(&outside, b"different config\n").unwrap();
        fs::hard_link(&outside, temp.path().join(".npmrc")).unwrap();
        let err = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap_err();
        assert!(err.to_string().contains("multiple hard links"));
        assert!(!temp.path().join("NuGet.Config").exists());
    }

    #[cfg(unix)]
    #[test]
    fn exact_crash_orphan_hard_link_recovers_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let (secrets, npm, nuget) = package_registry_specs();
        let rendered = render_package_registry_files(&secrets, &npm, &nuget).unwrap();
        let orphan = temp.path().join(".npmrc.ppduster.crash-orphan.tmp");
        fs::write(&orphan, &rendered.npmrc).unwrap();
        fs::hard_link(&orphan, temp.path().join(".npmrc")).unwrap();

        let result = apply_at(temp.path(), &secrets, &npm, &nuget).unwrap();

        assert!(result.contains("created 1"));
        assert_eq!(fs::read(&orphan).unwrap(), rendered.npmrc);
        assert_eq!(
            fs::read(temp.path().join("NuGet.Config")).unwrap(),
            rendered.nuget_config
        );
    }

    #[test]
    fn apply_requires_a_mixed_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let err = validate_package_workspace(temp.path()).unwrap_err();
        assert!(err.to_string().contains("Git repository root"));

        fs::create_dir(temp.path().join(".git")).unwrap();
        let err = validate_package_workspace(temp.path()).unwrap_err();
        assert!(err.to_string().contains("package.json"));
        fs::write(temp.path().join("package.json"), b"{}\n").unwrap();
        let err = validate_package_workspace(temp.path()).unwrap_err();
        assert!(err.to_string().contains(".NET solution or project"));
        fs::write(temp.path().join("Example.SLN"), b"\n").unwrap();
        validate_package_workspace(temp.path()).unwrap();
    }
}
