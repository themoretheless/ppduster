use crate::automation::package_registry;
use crate::automation::task::{
    Action, EncryptedSecretsSpec, NpmRegistryFileSpec, NugetRegistryFileSpec, Task,
    TrustRequirement,
};
use age::secrecy::SecretString;
use anyhow::{anyhow, bail, Context, Result};
use same_file::Handle;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use zeroize::{Zeroize, Zeroizing};

const VAULT_VERSION: u8 = 1;
const VAULT_KIND: &str = "github-packages";
const MAX_INPUT_BYTES: u64 = 64 * 1024;
const MAX_CIPHERTEXT_BYTES: u64 = 64 * 1024;
const MAX_PLAINTEXT_BYTES: u64 = 16 * 1024;
const MAX_USERNAME_BYTES: usize = 256;
const MAX_TOKEN_BYTES: usize = 4096;
const MIN_PASSPHRASE_CHARS: usize = 12;
const PRODUCTION_SCRYPT_WORK_FACTOR: u8 = 18;
#[cfg(not(test))]
const SCRYPT_WORK_FACTOR: u8 = PRODUCTION_SCRYPT_WORK_FACTOR;
// Unit tests exercise the same age format and validation paths with a cheaper KDF.
// Integration tests compile the library normally and therefore cover factor 18.
#[cfg(test)]
const SCRYPT_WORK_FACTOR: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageTool {
    Npm,
    Dotnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretInitMode {
    Interactive,
    JsonStdin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordMode {
    Interactive,
    Stdin,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultPayload {
    version: u8,
    kind: String,
    profile: String,
    workspace_id: String,
    username: String,
    token: String,
}

impl Drop for VaultPayload {
    fn drop(&mut self) {
        self.username.zeroize();
        self.token.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitStdinPayload {
    username: String,
    token: String,
    password: String,
    password_confirmation: String,
}

impl Drop for InitStdinPayload {
    fn drop(&mut self) {
        self.username.zeroize();
        self.token.zeroize();
        self.password.zeroize();
        self.password_confirmation.zeroize();
    }
}

struct RegistryTaskSpec<'a> {
    secrets: &'a EncryptedSecretsSpec,
    npm: &'a NpmRegistryFileSpec,
    nuget: &'a NugetRegistryFileSpec,
}

pub fn init_for_task(
    task: &Task,
    explicit_path: Option<&Path>,
    mode: SecretInitMode,
) -> Result<PathBuf> {
    ensure_vault_platform_security()?;
    let spec = registry_task_spec(task)?;
    let workspace_id = current_workspace_id()?;
    let target = resolve_vault_path(&spec.secrets.profile, &workspace_id, explicit_path)?;
    if fs::symlink_metadata(&target).is_ok() {
        bail!("secret vault already exists; refusing to overwrite it");
    }
    let (username, token, password) = read_init_input(mode)?;
    validate_secret_fields(&username, &token)?;

    let payload = VaultPayload {
        version: VAULT_VERSION,
        kind: VAULT_KIND.to_owned(),
        profile: spec.secrets.profile.clone(),
        workspace_id,
        username,
        token,
    };
    create_vault(&target, &payload, password)?;
    Ok(target)
}

pub fn exec_for_task(
    task: &Task,
    explicit_path: Option<&Path>,
    mode: PasswordMode,
    tool: PackageTool,
    args: &[OsString],
) -> Result<ExitStatus> {
    ensure_vault_platform_security()?;
    let spec = registry_task_spec(task)?;
    let workspace_id = current_workspace_id()?;
    validate_tool_args(tool, args, spec.secrets)?;

    if package_registry::is_satisfied(spec.secrets, spec.npm, spec.nuget)?.is_none() {
        bail!(
            "package registry files do not match the bundled task; run setup configuration first"
        );
    }

    let target = resolve_vault_path(&spec.secrets.profile, &workspace_id, explicit_path)?;
    let password = read_unlock_password(mode)?;
    let payload = unlock_vault(&target, &spec.secrets.profile, &workspace_id, password)?;

    let mut command = match tool {
        PackageTool::Npm => Command::new("npm"),
        PackageTool::Dotnet => Command::new("dotnet"),
    };
    command.args(args);
    sanitize_child_environment(&mut command, tool);
    let mut isolated_npm_configs = None;
    match tool {
        PackageTool::Npm => {
            let empty_user_config = tempfile::NamedTempFile::new()
                .context("create isolated temporary npm user configuration")?;
            let empty_global_config = tempfile::NamedTempFile::new()
                .context("create isolated temporary npm global configuration")?;
            secure_file(empty_user_config.as_file())?;
            secure_file(empty_global_config.as_file())?;
            command.arg("--ignore-scripts");
            command.env(&spec.secrets.token_env, &payload.token);
            command.env_remove(&spec.secrets.username_env);
            command.env("NPM_CONFIG_IGNORE_SCRIPTS", "true");
            command.env("NPM_CONFIG_USERCONFIG", empty_user_config.path());
            command.env("NPM_CONFIG_GLOBALCONFIG", empty_global_config.path());
            isolated_npm_configs = Some((empty_user_config, empty_global_config));
        }
        PackageTool::Dotnet => {
            command.arg("--configfile").arg("NuGet.Config");
            command.env(&spec.secrets.username_env, &payload.username);
            command.env(&spec.secrets.token_env, &payload.token);
            command.env("MSBUILDDISABLENODEREUSE", "1");
            command.env("DOTNET_CLI_USE_MSBUILD_SERVER", "0");
        }
    }

    let status = run_redacted_command(command, tool_name(tool), &payload.username, &payload.token)?;
    drop(isolated_npm_configs);
    Ok(status)
}

fn run_redacted_command(
    mut command: Command,
    tool: &str,
    username: &str,
    token: &str,
) -> Result<ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("start {tool} package command"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("capture package command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("capture package command stderr"))?;
    let stdout_patterns = secret_patterns(username, token);
    let stderr_patterns = secret_patterns(username, token);
    let stdout_thread = thread::spawn(move || {
        forward_redacted(stdout, io::stdout(), stdout_patterns)
            .context("forward package command stdout")
    });
    let stderr_thread = thread::spawn(move || {
        forward_redacted(stderr, io::stderr(), stderr_patterns)
            .context("forward package command stderr")
    });
    let status = child.wait().context("wait for package command");
    stdout_thread
        .join()
        .map_err(|_| anyhow!("package stdout forwarding failed"))??;
    stderr_thread
        .join()
        .map_err(|_| anyhow!("package stderr forwarding failed"))??;
    status
}

fn secret_patterns(username: &str, token: &str) -> Vec<Zeroizing<Vec<u8>>> {
    let mut patterns = [username, token]
        .into_iter()
        .filter(|value| !value.is_empty())
        .map(|value| Zeroizing::new(value.as_bytes().to_vec()))
        .collect::<Vec<_>>();
    patterns.sort_by_key(|value| std::cmp::Reverse(value.len()));
    patterns
}

fn forward_redacted(
    mut reader: impl Read,
    mut writer: impl Write,
    patterns: Vec<Zeroizing<Vec<u8>>>,
) -> Result<()> {
    let max_pattern = patterns.iter().map(|value| value.len()).max().unwrap_or(1);
    let mut pending = Zeroizing::new(Vec::new());
    let mut chunk = Zeroizing::new([0u8; 8192]);
    loop {
        let read = reader.read(&mut *chunk)?;
        let eof = read == 0;
        pending.extend_from_slice(&chunk[..read]);
        let keep = if eof {
            0
        } else {
            max_pattern.saturating_sub(1)
        };
        let process_limit = pending.len().saturating_sub(keep);
        let mut cursor = 0usize;
        let mut rendered = Vec::with_capacity(process_limit);
        while cursor < process_limit {
            if let Some(pattern) = patterns
                .iter()
                .find(|pattern| pending[cursor..].starts_with(pattern.as_slice()))
            {
                rendered.extend_from_slice(b"[REDACTED]");
                cursor += pattern.len();
            } else {
                rendered.push(pending[cursor]);
                cursor += 1;
            }
        }
        pending.drain(..cursor);
        writer.write_all(&rendered)?;
        writer.flush()?;
        if eof {
            break;
        }
    }
    Ok(())
}

pub fn default_vault_path(task: &Task) -> Result<PathBuf> {
    vault_path_for_task(task, None)
}

pub fn vault_path_for_task(task: &Task, explicit_path: Option<&Path>) -> Result<PathBuf> {
    let spec = registry_task_spec(task)?;
    let workspace_id = current_workspace_id()?;
    resolve_vault_path(&spec.secrets.profile, &workspace_id, explicit_path)
}

fn registry_task_spec(task: &Task) -> Result<RegistryTaskSpec<'_>> {
    task.validate()
        .map_err(|_| anyhow!("invalid package registry task"))?;
    if task.trust != TrustRequirement::BundledOnly {
        bail!("package secrets are available only to bundled-only tasks");
    }
    let mut found = None;
    for step in &task.steps {
        if let Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } = &step.action
        {
            if found.is_some() {
                bail!("package registry task must contain exactly one registry action");
            }
            found = Some(RegistryTaskSpec {
                secrets,
                npm,
                nuget,
            });
        }
    }
    found.ok_or_else(|| anyhow!("task does not configure package registry files"))
}

fn resolve_vault_path(
    profile: &str,
    workspace_id: &str,
    explicit_path: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .context("resolve current directory")?
                .join(path)
        };
        let parent = absolute
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| anyhow!("secret vault path requires a parent directory"))?;
        if !parent.is_dir() {
            bail!("explicit secret vault parent directory must already exist");
        }
        reject_symlink_or_reparse(parent, "secret vault parent")?;
        validate_secure_directory(parent)?;
        if absolute
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            bail!("secret vault path must not contain parent-directory components");
        }
        let parent = if parent.exists() {
            parent.canonicalize().with_context(|| {
                format!("canonicalize secret vault directory {}", parent.display())
            })?
        } else {
            parent.to_path_buf()
        };
        let target = parent.join(
            absolute
                .file_name()
                .ok_or_else(|| anyhow!("secret vault path requires a file name"))?,
        );
        ensure_outside_current_repo(&target)?;
        return Ok(target);
    }

    let base = dirs::config_dir().ok_or_else(|| anyhow!("user config directory unavailable"))?;
    let base = canonicalize_with_missing_tail(&base)?;
    reject_existing_app_path_components(&base)?;
    let target = base
        .join("ppduster")
        .join("secrets")
        .join("v1")
        .join(format!("{profile}-{}.age", &workspace_id[..16]));
    ensure_outside_current_repo(&target)?;
    Ok(target)
}

fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut ancestor = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    bail!("secret vault path contains a symlink or non-directory component");
                }
                #[cfg(windows)]
                reject_windows_reparse_point(&metadata)?;
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| anyhow!("could not resolve user config directory"))?
                    .to_os_string();
                missing.push(name);
                if !ancestor.pop() {
                    bail!("could not resolve user config directory");
                }
            }
            Err(err) => return Err(err).context("inspect user config directory"),
        }
    }
    let mut canonical = ancestor
        .canonicalize()
        .context("canonicalize user config directory ancestor")?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn reject_existing_app_path_components(base: &Path) -> Result<()> {
    let mut current = base.to_path_buf();
    for component in ["ppduster", "secrets", "v1"] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    bail!("secret vault path contains a symlink or non-directory component");
                }
                #[cfg(windows)]
                reject_windows_reparse_point(&metadata)?;
                validate_secure_directory(&current)?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(err).context("inspect secret vault path"),
        }
    }
    Ok(())
}

fn reject_symlink_or_reparse(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{label} must not be a symlink");
    }
    #[cfg(windows)]
    reject_windows_reparse_point(&metadata)?;
    Ok(())
}

fn current_workspace_id() -> Result<String> {
    let root = std::env::current_dir()
        .context("resolve package workspace")?
        .canonicalize()
        .context("canonicalize package workspace")?;
    if !root.join(".git").exists() {
        bail!("package secret vault commands must run from a Git repository root");
    }
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(root.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(root.to_string_lossy().to_ascii_lowercase().as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

fn ensure_outside_current_repo(target: &Path) -> Result<()> {
    if let Ok(repo) = std::env::current_dir().and_then(|path| path.canonicalize()) {
        if repo.join(".git").exists() && target.starts_with(&repo) {
            bail!("secret vault must be stored outside the Git repository");
        }
    }
    Ok(())
}

fn read_init_input(mode: SecretInitMode) -> Result<(String, String, SecretString)> {
    match mode {
        SecretInitMode::Interactive => {
            if !io::stdin().is_terminal() {
                bail!("interactive secret input requires a terminal");
            }
            eprint!("GitHub Packages username: ");
            io::stderr().flush().context("flush username prompt")?;
            let mut username = String::new();
            io::stdin()
                .read_line(&mut username)
                .context("read GitHub Packages username")?;
            trim_line_ending(&mut username);
            if username.len() > MAX_USERNAME_BYTES {
                bail!("invalid GitHub Packages username");
            }
            let token = Zeroizing::new(
                rpassword::prompt_password("GitHub Packages token: ")
                    .map_err(|_| anyhow!("could not read hidden token"))?,
            );
            let password = Zeroizing::new(
                rpassword::prompt_password("Vault password: ")
                    .map_err(|_| anyhow!("could not read hidden vault password"))?,
            );
            let confirmation = Zeroizing::new(
                rpassword::prompt_password("Repeat vault password: ")
                    .map_err(|_| anyhow!("could not read hidden vault password confirmation"))?,
            );
            validate_password(&password, &confirmation)?;
            Ok((
                username,
                token.to_string(),
                SecretString::from(password.to_string()),
            ))
        }
        SecretInitMode::JsonStdin => {
            if io::stdin().is_terminal() {
                bail!("JSON secret input must be redirected from a pipe or file");
            }
            let raw = read_bounded_string(&mut io::stdin(), MAX_INPUT_BYTES)?;
            let input: InitStdinPayload =
                serde_json::from_str(&raw).map_err(|_| anyhow!("invalid package secret input"))?;
            validate_password(&input.password, &input.password_confirmation)?;
            Ok((
                input.username.clone(),
                input.token.clone(),
                SecretString::from(input.password.clone()),
            ))
        }
    }
}

fn read_unlock_password(mode: PasswordMode) -> Result<SecretString> {
    let mut password = match mode {
        PasswordMode::Interactive => {
            if !io::stdin().is_terminal() {
                bail!("interactive vault unlock requires a terminal");
            }
            rpassword::prompt_password("Vault password: ")
                .map_err(|_| anyhow!("could not read hidden vault password"))?
        }
        PasswordMode::Stdin => {
            if io::stdin().is_terminal() {
                bail!("vault password stdin must be redirected from a pipe or file");
            }
            read_bounded_line(&mut io::stdin(), 8192)?
        }
    };
    if password.is_empty() || password.chars().any(char::is_control) {
        password.zeroize();
        bail!("invalid vault password input");
    }
    Ok(SecretString::from(password))
}

fn validate_password(password: &str, confirmation: &str) -> Result<()> {
    if password != confirmation {
        bail!("vault password confirmation does not match");
    }
    if password.chars().count() < MIN_PASSPHRASE_CHARS || password.chars().any(char::is_control) {
        bail!("vault password must contain at least 12 non-control characters");
    }
    Ok(())
}

fn validate_secret_fields(username: &str, token: &str) -> Result<()> {
    if username.is_empty()
        || username.len() > MAX_USERNAME_BYTES
        || username.chars().any(char::is_control)
    {
        bail!("invalid GitHub Packages username");
    }
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_control) {
        bail!("invalid GitHub Packages token");
    }
    Ok(())
}

fn create_vault(target: &Path, payload: &VaultPayload, password: SecretString) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("secret vault path requires a parent directory"))?;
    let parent = prepare_private_directory(parent)?;
    let target = parent.join(
        target
            .file_name()
            .ok_or_else(|| anyhow!("secret vault path requires a file name"))?,
    );
    ensure_outside_current_repo(&target)?;
    if fs::symlink_metadata(&target).is_ok() {
        bail!("secret vault already exists; refusing to overwrite it");
    }

    let plaintext = Zeroizing::new(
        serde_json::to_vec(payload).map_err(|_| anyhow!("could not encode package secrets"))?,
    );
    if plaintext.len() as u64 > MAX_PLAINTEXT_BYTES {
        bail!("package secrets exceed the supported size");
    }
    let mut recipient = age::scrypt::Recipient::new(password);
    recipient.set_work_factor(SCRYPT_WORK_FACTOR);
    let ciphertext = age::encrypt(&recipient, &plaintext)
        .map_err(|_| anyhow!("could not encrypt package secrets"))?;
    if ciphertext.len() as u64 > MAX_CIPHERTEXT_BYTES {
        bail!("encrypted package secrets exceed the supported size");
    }

    let mut staged = tempfile::Builder::new()
        .prefix(".ppduster-vault-")
        .suffix(".tmp")
        .tempfile_in(&parent)
        .with_context(|| format!("stage encrypted vault in {}", parent.display()))?;
    secure_file(staged.as_file())?;
    staged
        .write_all(&ciphertext)
        .context("write encrypted package secret vault")?;
    staged
        .flush()
        .context("flush encrypted package secret vault")?;
    staged
        .as_file()
        .sync_all()
        .context("sync encrypted package secret vault")?;
    staged
        .persist_noclobber(&target)
        .map_err(|_| anyhow!("secret vault already exists or could not be created"))?;
    sync_parent_directory(&parent)?;
    Ok(())
}

fn prepare_private_directory(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        reject_symlink_or_reparse(path, "secret vault directory")?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize secret vault directory {}", path.display()))?;
        validate_secure_directory(&canonical)?;
        return Ok(canonical);
    }

    let mut missing = Vec::new();
    let mut ancestor = path.to_path_buf();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| anyhow!("could not resolve secret vault directory"))?
            .to_os_string();
        missing.push(name);
        if !ancestor.pop() {
            bail!("could not resolve secret vault directory");
        }
    }
    reject_symlink_or_reparse(&ancestor, "secret vault ancestor")?;
    let mut current = ancestor
        .canonicalize()
        .with_context(|| format!("canonicalize secret vault ancestor {}", ancestor.display()))?;
    for name in missing.into_iter().rev() {
        let next = current.join(name);
        match create_private_directory(&next) {
            Ok(()) => secure_directory(&next)?,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                validate_secure_directory(&next)?;
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("create secret vault directory {}", next.display()));
            }
        }
        let metadata = fs::symlink_metadata(&next)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("secret vault path contains a non-directory or symlink component");
        }
        #[cfg(windows)]
        reject_windows_reparse_point(&metadata)?;
        current = next
            .canonicalize()
            .with_context(|| format!("canonicalize secret vault directory {}", next.display()))?;
    }
    validate_secure_directory(&current)?;
    Ok(current)
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    builder.create(path)
}

fn ensure_vault_platform_security() -> Result<()> {
    #[cfg(windows)]
    bail!("encrypted package vault is unavailable until owner-only Windows ACL support is enabled");
    #[cfg(not(windows))]
    Ok(())
}

fn unlock_vault(
    target: &Path,
    profile: &str,
    workspace_id: &str,
    password: SecretString,
) -> Result<VaultPayload> {
    unlock_vault_inner(target, profile, workspace_id, password).map_err(|_| {
        anyhow!("could not unlock package secrets (wrong password or invalid file) [secret_unlock_failed]")
    })
}

fn unlock_vault_inner(
    target: &Path,
    profile: &str,
    workspace_id: &str,
    password: SecretString,
) -> Result<VaultPayload> {
    let mut file = open_existing_vault(target)?;
    let mut ciphertext = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CIPHERTEXT_BYTES + 1)
        .read_to_end(&mut ciphertext)?;
    if ciphertext.is_empty() || ciphertext.len() as u64 > MAX_CIPHERTEXT_BYTES {
        bail!("invalid ciphertext size");
    }
    validate_scrypt_work_factor(&ciphertext)?;
    let decryptor = age::Decryptor::new_buffered(ciphertext.as_slice())?;
    if !decryptor.is_scrypt() {
        bail!("not a passphrase age file");
    }
    let mut identity = age::scrypt::Identity::new(password);
    identity.set_max_work_factor(SCRYPT_WORK_FACTOR);
    let mut reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
    let mut plaintext = Zeroizing::new(Vec::new());
    reader
        .by_ref()
        .take(MAX_PLAINTEXT_BYTES + 1)
        .read_to_end(&mut plaintext)?;
    if plaintext.len() as u64 > MAX_PLAINTEXT_BYTES {
        bail!("plaintext too large");
    }
    let payload: VaultPayload = serde_json::from_slice(&plaintext)?;
    if payload.version != VAULT_VERSION
        || payload.kind != VAULT_KIND
        || payload.profile != profile
        || payload.workspace_id != workspace_id
    {
        bail!("vault metadata mismatch");
    }
    validate_secret_fields(&payload.username, &payload.token)?;
    Ok(payload)
}

fn open_existing_vault(target: &Path) -> Result<File> {
    if let Some(parent) = target.parent() {
        validate_secure_directory(parent)?;
    }
    let path_metadata = fs::symlink_metadata(target)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
        bail!("invalid vault file type");
    }
    #[cfg(windows)]
    reject_windows_reparse_point(&path_metadata)?;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(target)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CIPHERTEXT_BYTES {
        bail!("invalid vault file metadata");
    }
    #[cfg(windows)]
    reject_windows_reparse_point(&metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let links_are_safe = metadata.nlink() == 1
            || (metadata.nlink() == 2 && has_recoverable_staging_link(target, &file)?);
        if !links_are_safe || metadata.mode() & 0o077 != 0 {
            bail!("insecure vault metadata");
        }
    }
    let opened = Handle::from_file(file.try_clone()?)?;
    let current = Handle::from_path(target)?;
    if opened != current {
        bail!("secret vault changed while it was opened");
    }
    Ok(file)
}

#[cfg(unix)]
fn has_recoverable_staging_link(target: &Path, file: &File) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let identity = Handle::from_file(file.try_clone()?)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("vault path has no parent"))?;
    let mut matching = 0usize;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if entry.path() == target {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".ppduster-vault-") || !name.ends_with(".tmp") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.mode() & 0o077 != 0
        {
            continue;
        }
        if Handle::from_path(entry.path())? == identity {
            matching += 1;
        }
    }
    Ok(matching == 1)
}

fn validate_scrypt_work_factor(ciphertext: &[u8]) -> Result<()> {
    let header_end = ciphertext
        .windows(4)
        .position(|window| window == b"--- ")
        .unwrap_or(ciphertext.len().min(4096));
    let header = &ciphertext[..header_end];
    let mut factors = header.split(|byte| *byte == b'\n').filter_map(|line| {
        line.strip_prefix(b"-> scrypt ").and_then(|rest| {
            let mut parts = rest.split(|byte| *byte == b' ');
            let _salt = parts.next()?;
            let factor = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            std::str::from_utf8(factor).ok()?.parse::<u8>().ok()
        })
    });
    let factor = factors
        .next()
        .ok_or_else(|| anyhow!("missing scrypt stanza"))?;
    if factors.next().is_some() || factor != SCRYPT_WORK_FACTOR {
        bail!("unsupported scrypt work factor");
    }
    Ok(())
}

fn validate_tool_args(
    tool: PackageTool,
    args: &[OsString],
    secrets: &EncryptedSecretsSpec,
) -> Result<()> {
    let verb = args
        .first()
        .and_then(|arg| arg.to_str())
        .ok_or_else(|| anyhow!("package command requires a supported verb"))?;
    if args.len() != 1 {
        bail!("package secret commands accept only the package-manager verb");
    }
    let supported = match tool {
        PackageTool::Npm => matches!(verb, "ci" | "install"),
        PackageTool::Dotnet => verb == "restore",
    };
    if !supported {
        bail!("unsupported package command; use npm ci/install or dotnet restore");
    }
    let username_env = secrets.username_env.to_ascii_lowercase();
    let token_env = secrets.token_env.to_ascii_lowercase();
    let forbidden = [
        "_authtoken",
        "cleartextpassword",
        "ignore-scripts",
        "foreground-scripts",
        "userconfig",
        "globalconfig",
        "registry",
        username_env.as_str(),
        token_env.as_str(),
    ];
    for arg in args {
        let Some(value) = arg.to_str() else {
            bail!("package command arguments must be valid text");
        };
        let lowered = value.to_ascii_lowercase();
        let overrides_dotnet_source = matches!(tool, PackageTool::Dotnet)
            && (matches!(lowered.as_str(), "--source" | "-s")
                || lowered.starts_with("--source=")
                || lowered.contains("configfile")
                || lowered.contains("restoresources")
                || lowered.contains("restoreadditionalprojectsources"));
        if value.chars().any(char::is_control)
            || forbidden.iter().any(|needle| lowered.contains(needle))
            || overrides_dotnet_source
        {
            bail!("credential-bearing package command arguments are forbidden");
        }
    }
    Ok(())
}

fn sanitize_child_environment(command: &mut Command, tool: PackageTool) {
    for (key, _) in std::env::vars_os() {
        let Some(key_text) = key.to_str() else {
            continue;
        };
        let upper = key_text.to_ascii_uppercase();
        let common_injection = matches!(
            upper.as_str(),
            "LD_PRELOAD" | "LD_LIBRARY_PATH" | "LD_AUDIT"
        ) || upper.starts_with("DYLD_");
        let tool_injection = match tool {
            PackageTool::Npm => {
                upper.starts_with("NPM_CONFIG_")
                    || matches!(upper.as_str(), "NODE_OPTIONS" | "NODE_PATH")
            }
            PackageTool::Dotnet => {
                upper.starts_with("MSBUILD")
                    || matches!(
                        upper.as_str(),
                        "DOTNET_STARTUP_HOOKS"
                            | "DOTNET_ADDITIONAL_DEPS"
                            | "DOTNET_SHARED_STORE"
                            | "DOTNET_CLI_USE_MSBUILD_SERVER"
                            | "NUGET_PLUGIN_PATHS"
                            | "NUGET_CREDENTIALPROVIDERS_PATH"
                            | "RESTORESOURCES"
                            | "RESTOREADDITIONALPROJECTSOURCES"
                            | "RESTORECONFIGFILE"
                            | "RESTOREFALLBACKFOLDERS"
                            | "RESTOREADDITIONALPROJECTFALLBACKFOLDERS"
                    )
            }
        };
        if common_injection || tool_injection {
            command.env_remove(key);
        }
    }
}

#[cfg(test)]
fn redact_exact(input: &[u8], secrets: &[&[u8]]) -> Vec<u8> {
    let mut output = input.to_vec();
    let mut secrets = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        let mut cursor = 0;
        while cursor + secret.len() <= output.len() {
            if &output[cursor..cursor + secret.len()] == secret {
                output.splice(cursor..cursor + secret.len(), b"[REDACTED]".iter().copied());
                cursor += b"[REDACTED]".len();
            } else {
                cursor += 1;
            }
        }
    }
    output
}

fn read_bounded_string(reader: &mut impl Read, max: u64) -> Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .take(max + 1)
        .read_to_end(&mut bytes)
        .context("read secret input")?;
    if bytes.len() as u64 > max {
        bail!("secret input exceeds the supported size");
    }
    let value = String::from_utf8(bytes.to_vec()).map_err(|_| anyhow!("invalid secret input"))?;
    Ok(Zeroizing::new(value))
}

fn read_bounded_line(reader: &mut impl Read, max: u64) -> Result<String> {
    let mut value = read_bounded_string(reader, max)?;
    trim_line_ending(&mut value);
    Ok(value.to_string())
}

fn trim_line_ending(value: &mut String) {
    while matches!(value.chars().last(), Some('\n' | '\r')) {
        value.pop();
    }
}

fn secure_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect secret vault directory {}", _path.display()))?;
    }
    Ok(())
}

fn validate_secure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect secret vault directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("secret vault parent must be a regular directory");
    }
    #[cfg(windows)]
    reject_windows_reparse_point(&metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            bail!("secret vault directory must be accessible only by its owner");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn reject_windows_reparse_point(metadata: &fs::Metadata) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!("secret vault paths must not contain Windows reparse points");
    }
    Ok(())
}

fn secure_file(_file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("protect encrypted package secret vault")?;
    }
    Ok(())
}

fn sync_parent_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(_path)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("sync secret vault directory {}", _path.display()))?;
    Ok(())
}

fn tool_name(tool: PackageTool) -> &'static str {
    match tool {
        PackageTool::Npm => "npm",
        PackageTool::Dotnet => "dotnet",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WORKSPACE_ID: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn exact_redaction_covers_every_occurrence() {
        assert_eq!(PRODUCTION_SCRYPT_WORK_FACTOR, 18);
        assert_eq!(
            redact_exact(b"before-token-token-after", &[b"token"]),
            b"before-[REDACTED]-[REDACTED]-after"
        );
    }

    #[test]
    fn streaming_redaction_covers_chunk_boundaries() {
        struct Chunked<'a> {
            input: &'a [u8],
            offset: usize,
            chunk_size: usize,
        }

        impl Read for Chunked<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.offset == self.input.len() {
                    return Ok(0);
                }
                let count = self
                    .chunk_size
                    .min(output.len())
                    .min(self.input.len() - self.offset);
                output[..count].copy_from_slice(&self.input[self.offset..self.offset + count]);
                self.offset += count;
                Ok(count)
            }
        }

        let mut bytes = vec![b'x'; 8190];
        bytes.extend_from_slice(b"prefix-long-token|prefix|prefix-long-token");
        let input = Chunked {
            input: &bytes,
            offset: 0,
            chunk_size: 8192,
        };
        let mut output = Vec::new();
        forward_redacted(
            input,
            &mut output,
            secret_patterns("prefix", "prefix-long-token"),
        )
        .unwrap();

        let mut expected = vec![b'x'; 8190];
        expected.extend_from_slice(b"[REDACTED]|[REDACTED]|[REDACTED]");
        assert_eq!(output, expected);
    }

    #[test]
    fn age_ciphertexts_are_randomized_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let payload = VaultPayload {
            version: VAULT_VERSION,
            kind: VAULT_KIND.into(),
            profile: "test-profile".into(),
            workspace_id: TEST_WORKSPACE_ID.into(),
            username: "secret-user-canary".into(),
            token: "secret-token-canary".into(),
        };
        let vault_dir = dir.path().join("vault");
        let first = vault_dir.join("first.age");
        let second = vault_dir.join("second.age");
        for target in [&first, &second] {
            create_vault(
                target,
                &payload,
                SecretString::from("a sufficiently long password".to_owned()),
            )
            .unwrap();
        }
        let first_bytes = fs::read(&first).unwrap();
        let second_bytes = fs::read(&second).unwrap();
        assert_ne!(first_bytes, second_bytes);
        for bytes in [&first_bytes, &second_bytes] {
            assert!(!bytes
                .windows("secret-user-canary".len())
                .any(|w| w == b"secret-user-canary"));
            assert!(!bytes
                .windows("secret-token-canary".len())
                .any(|w| w == b"secret-token-canary"));
        }
        let unlocked = unlock_vault(
            &first,
            "test-profile",
            TEST_WORKSPACE_ID,
            SecretString::from("a sufficiently long password".to_owned()),
        )
        .unwrap();
        assert_eq!(unlocked.username, "secret-user-canary");
        assert_eq!(unlocked.token, "secret-token-canary");
    }

    #[test]
    fn wrong_password_and_tamper_have_same_redacted_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("vault").join("vault.age");
        let payload = VaultPayload {
            version: VAULT_VERSION,
            kind: VAULT_KIND.into(),
            profile: "test-profile".into(),
            workspace_id: TEST_WORKSPACE_ID.into(),
            username: "user".into(),
            token: "token".into(),
        };
        create_vault(
            &target,
            &payload,
            SecretString::from("correct long password".to_owned()),
        )
        .unwrap();
        let wrong = unlock_vault(
            &target,
            "test-profile",
            TEST_WORKSPACE_ID,
            SecretString::from("incorrect long password".to_owned()),
        )
        .err()
        .unwrap()
        .to_string();
        let wrong_workspace = unlock_vault(
            &target,
            "test-profile",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            SecretString::from("correct long password".to_owned()),
        )
        .err()
        .unwrap()
        .to_string();
        let mut tampered = fs::read(&target).unwrap();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        fs::write(&target, tampered).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let damaged = unlock_vault(
            &target,
            "test-profile",
            TEST_WORKSPACE_ID,
            SecretString::from("correct long password".to_owned()),
        )
        .err()
        .unwrap()
        .to_string();
        assert_eq!(wrong, damaged);
        assert_eq!(wrong, wrong_workspace);
        assert_eq!(wrong, "could not unlock package secrets (wrong password or invalid file) [secret_unlock_failed]");
    }

    #[test]
    fn init_is_create_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("vault.age");
        fs::write(&target, b"existing").unwrap();
        let payload = VaultPayload {
            version: VAULT_VERSION,
            kind: VAULT_KIND.into(),
            profile: "test-profile".into(),
            workspace_id: TEST_WORKSPACE_ID.into(),
            username: "user".into(),
            token: "token".into(),
        };
        assert!(create_vault(
            &target,
            &payload,
            SecretString::from("a sufficiently long password".to_owned()),
        )
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
    }

    #[cfg(unix)]
    #[test]
    fn encrypted_crash_staging_hardlink_remains_unlockable() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("vault").join("vault.age");
        let payload = VaultPayload {
            version: VAULT_VERSION,
            kind: VAULT_KIND.into(),
            profile: "test-profile".into(),
            workspace_id: TEST_WORKSPACE_ID.into(),
            username: "user".into(),
            token: "token".into(),
        };
        create_vault(
            &target,
            &payload,
            SecretString::from("a sufficiently long password".to_owned()),
        )
        .unwrap();
        let orphan = target.parent().unwrap().join(".ppduster-vault-crash.tmp");
        fs::hard_link(&target, &orphan).unwrap();

        let unlocked = unlock_vault(
            &target,
            "test-profile",
            TEST_WORKSPACE_ID,
            SecretString::from("a sufficiently long password".to_owned()),
        )
        .unwrap();

        assert_eq!(unlocked.username, "user");
        assert_eq!(unlocked.token, "token");
    }
}
