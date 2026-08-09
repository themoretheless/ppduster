use crate::automation::package_registry;
use crate::automation::task::{Action, AuthPolicy, ElevationPolicy, ShellMode, Step, Task};
use crate::rules::expand_path_template;
use crate::safety::{is_safe_rule_root, stays_under_root};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub apply: bool,
    pub allow_shell: bool,
    pub allow_elevation: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionPlan {
    pub step_id: String,
    pub step_name: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Pending,
    Running,
    WaitingForAttention,
    Skipped,
    Satisfied,
    Applied,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepLogEntry {
    pub step_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub step_id: String,
    pub step_name: String,
    pub summary: String,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<StepLogEntry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionOutcome {
    Planned { action: ActionPlan },
    AlreadySatisfied { reason: String },
    Applied { summary: String },
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub plans: Vec<ActionPlan>,
    pub outcomes: Vec<ActionOutcome>,
    pub steps: Vec<StepReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Default)]
struct AuthState {
    git_authenticated: bool,
    sudo_authenticated: bool,
}

pub fn run_task(task: &Task, opts: &RunOptions) -> Result<RunReport> {
    task.validate().map_err(AutomationError::Message)?;
    let mut plans = Vec::new();
    let mut outcomes = Vec::new();
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    let mut auth_state = AuthState::default();
    let mut halted = false;

    for step in &task.steps {
        if halted {
            let plan = plan_step(step)?;
            plans.push(plan.clone());
            outcomes.push(ActionOutcome::Blocked);
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: plan.summary.clone(),
                status: StepStatus::Skipped,
                prerequisites: plan.prerequisites.clone(),
                logs: vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: "skipped after earlier failure".into(),
                }],
            });
            continue;
        }
        enforce_step_policy(step, opts)?;
        let satisfaction = is_satisfied(step, opts.apply)?;
        if let Some(reason) = satisfaction {
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: plan_summary(step)?,
                status: StepStatus::Satisfied,
                prerequisites: prerequisites_for_step(step),
                logs: vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: reason.clone(),
                }],
            });
            outcomes.push(ActionOutcome::AlreadySatisfied { reason });
            continue;
        }
        let plan = plan_step(step)?;
        plans.push(plan.clone());
        let step_idx = steps.len();
        steps.push(StepReport {
            step_id: step.id.clone(),
            step_name: step_name(step),
            summary: plan.summary.clone(),
            status: StepStatus::Pending,
            prerequisites: plan.prerequisites.clone(),
            logs: vec![StepLogEntry {
                step_id: step.id.clone(),
                message: "queued".into(),
            }],
        });
        if opts.apply {
            if step_requires_auth_prompt(step, &auth_state)? {
                steps[step_idx].status = StepStatus::WaitingForAttention;
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: "waiting for authorization".into(),
                });
                if let Err(err) = ensure_auth(step, &mut auth_state) {
                    let message = err.to_string();
                    steps[step_idx].status = StepStatus::Failed;
                    steps[step_idx].logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {message}"),
                    });
                    errors.push(message.clone());
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                }
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: "authorization granted; resuming".into(),
                });
            }
            steps[step_idx].status = StepStatus::Running;
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: "running".into(),
            });
            let summary = match apply_step(step) {
                Ok(summary) => summary,
                Err(err) => {
                    let message = err.to_string();
                    steps[step_idx].status = StepStatus::Failed;
                    steps[step_idx].logs.push(StepLogEntry {
                        step_id: step.id.clone(),
                        message: format!("failed: {message}"),
                    });
                    errors.push(message.clone());
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                    continue;
                }
            };
            steps[step_idx].status = StepStatus::Applied;
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: summary.clone(),
            });
            outcomes.push(ActionOutcome::Applied { summary });
            continue;
        }
        outcomes.push(ActionOutcome::Planned { action: plan });
    }

    Ok(RunReport {
        task_id: task.id.clone(),
        plans,
        outcomes,
        steps,
        errors,
    })
}

fn enforce_step_policy(step: &Step, opts: &RunOptions) -> Result<()> {
    if matches!(step.allow_elevation, ElevationPolicy::Allow) && !opts.allow_elevation {
        return Err(AutomationError::Message(format!(
            "step {} requires --allow-elevation",
            step.id
        ))
        .into());
    }
    if let Action::RunCommand { shell, .. } = &step.action {
        if matches!(shell, ShellMode::Allow) && !opts.allow_shell {
            return Err(AutomationError::Message(format!(
                "step {} requires --allow-shell",
                step.id
            ))
            .into());
        }
    }
    validate_destinations(step)?;
    Ok(())
}

fn validate_destinations(step: &Step) -> Result<()> {
    match &step.action {
        Action::GitClone { dest, .. }
        | Action::DownloadFile { dest, .. }
        | Action::ExtractArchive { dest, .. } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            if !is_safe_rule_root(parent_or_self(&path)) {
                bail!(
                    "step {} destination {} blocked by safety",
                    step.id,
                    path.display()
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn parent_or_self(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

fn plan_step(step: &Step) -> Result<ActionPlan> {
    let prerequisites = prerequisites_for_step(step);
    let summary = plan_summary(step)?;
    Ok(ActionPlan {
        step_id: step.id.clone(),
        step_name: step_name(step),
        summary,
        prerequisites,
    })
}

fn plan_summary(step: &Step) -> Result<String> {
    Ok(match &step.action {
        Action::GitClone { repo, dest, branch } => format!(
            "git clone {} {}{} with hooks disabled and no submodules",
            repo,
            dest,
            branch
                .as_ref()
                .map(|b| format!(" (branch {})", b))
                .unwrap_or_default()
        ),
        Action::BrewInstall { package, cask } => {
            format!(
                "brew install {}{}",
                if *cask { "--cask " } else { "" },
                package
            )
        }
        Action::RunCommand {
            program,
            args,
            cwd,
            shell,
            ..
        } => format!(
            "run {} {:?}{}{}",
            program,
            args,
            cwd.as_ref()
                .map(|d| format!(" in {}", d))
                .unwrap_or_default(),
            if matches!(shell, ShellMode::Allow) {
                " with shell"
            } else {
                ""
            }
        ),
        Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } => package_registry::plan_summary(secrets, npm, nuget)?,
        Action::DownloadFile { url, dest, .. } => {
            format!("download {} to {} with sha256 verification", url, dest)
        }
        Action::ExtractArchive { src, dest } => {
            format!("extract {} into {} with traversal protection", src, dest)
        }
        Action::InstallDmg { dmg, app_name } => format!(
            "mount {} read-only, validate signature, install {}",
            dmg,
            app_name.as_deref().unwrap_or("application")
        ),
        Action::InstallPkg { pkg, target } => format!(
            "validate pkg signature for {} and install to {}",
            pkg,
            target.as_deref().unwrap_or("/")
        ),
    })
}

fn prerequisites_for_step(step: &Step) -> Vec<String> {
    let mut prerequisites = Vec::new();
    match step.auth {
        AuthPolicy::None => {}
        AuthPolicy::GitCredential => prerequisites.push(
            "authenticate with git once if credentials are not already available; reuse the existing credential helper or SSH agent afterwards"
                .into(),
        ),
        AuthPolicy::Sudo => prerequisites.push(
            "authenticate with sudo once if the session does not already have an active sudo timestamp; later elevated steps can reuse it until the sudo timeout expires"
                .into(),
        ),
    }
    prerequisites
}

fn step_name(step: &Step) -> String {
    if step.name.trim().is_empty() {
        step.id.clone()
    } else {
        step.name.clone()
    }
}

fn ensure_auth(step: &Step, state: &mut AuthState) -> Result<()> {
    match step.auth {
        AuthPolicy::None => Ok(()),
        AuthPolicy::GitCredential => {
            if state.git_authenticated || git_auth_ready() {
                state.git_authenticated = true;
                return Ok(());
            }
            prompt_once(
                "Git authentication is required. Press Enter to continue and complete the normal git credential prompt if it appears.",
            )?;
            state.git_authenticated = true;
            Ok(())
        }
        AuthPolicy::Sudo => {
            if state.sudo_authenticated || sudo_auth_ready()? {
                state.sudo_authenticated = true;
                return Ok(());
            }
            prompt_once(
                "sudo authentication is required. Press Enter to continue; you may be prompted for your password once.",
            )?;
            Command::new("sudo")
                .arg("-v")
                .status()
                .context("refresh sudo credentials")?
                .exit_ok("refresh sudo credentials")?;
            state.sudo_authenticated = true;
            Ok(())
        }
    }
}

fn step_requires_auth_prompt(step: &Step, state: &AuthState) -> Result<bool> {
    match step.auth {
        AuthPolicy::None => Ok(false),
        AuthPolicy::GitCredential => Ok(!(state.git_authenticated || git_auth_ready())),
        AuthPolicy::Sudo => Ok(!(state.sudo_authenticated || sudo_auth_ready()?)),
    }
}

fn git_auth_ready() -> bool {
    std::env::var_os("SSH_AUTH_SOCK").is_some() || git_has_credential_helper()
}

fn git_has_credential_helper() -> bool {
    Command::new("git")
        .args(["config", "--get-all", "credential.helper"])
        .output()
        .map(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
        .unwrap_or(false)
}

fn sudo_auth_ready() -> Result<bool> {
    Ok(Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .context("check sudo credential cache")?
        .success())
}

fn prompt_once(message: &str) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "interactive authorization is required, but stdin is not a TTY; rerun in an interactive terminal"
        );
    }
    eprint!("{message}\nPress Enter to continue: ");
    io::stderr().flush().context("flush auth prompt")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read auth confirmation")?;
    Ok(())
}

fn apply_step(step: &Step) -> Result<String> {
    match &step.action {
        Action::GitClone { repo, dest, branch } => apply_git_clone(repo, dest, branch.as_deref()),
        Action::BrewInstall { package, cask } => apply_brew_install(package, *cask),
        Action::RunCommand {
            program,
            args,
            cwd,
            env,
            shell,
        } => apply_run_command(program, args, cwd.as_deref(), env, *shell),
        Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } => package_registry::apply(secrets, npm, nuget),
        Action::DownloadFile {
            url,
            dest,
            checksum,
        } => apply_download_file(url, dest, checksum),
        Action::ExtractArchive { src, dest } => apply_extract_archive(src, dest),
        Action::InstallDmg { dmg, app_name } => apply_install_dmg(dmg, app_name.as_deref()),
        Action::InstallPkg { pkg, target } => apply_install_pkg(pkg, target.as_deref()),
    }
}

fn apply_git_clone(repo: &str, dest: &str, branch: Option<&str>) -> Result<String> {
    let dest_path = expand_required_path(dest)?;
    if dest_path.exists() {
        bail!("clone destination already exists: {}", dest_path.display());
    }
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }
    let mut command = Command::new("git");
    command.arg("clone");
    command.arg("--config").arg("core.hooksPath=/dev/null");
    command.arg("--recurse-submodules=no");
    if let Some(branch) = branch {
        command.arg("--branch").arg(branch);
    }
    command.arg(repo).arg(&dest_path);
    command
        .status()
        .with_context(|| format!("clone {} into {}", repo, dest_path.display()))?
        .exit_ok("git clone")?;
    Ok(format!("cloned {} into {}", repo, dest_path.display()))
}

fn apply_brew_install(package: &str, cask: bool) -> Result<String> {
    let already_installed = if cask {
        Command::new("brew")
            .args(["list", "--cask", package])
            .status()
            .with_context(|| format!("check brew cask {}", package))?
            .success()
    } else {
        Command::new("brew")
            .args(["list", package])
            .status()
            .with_context(|| format!("check brew package {}", package))?
            .success()
    };
    if already_installed {
        return Ok(format!("brew package already installed: {}", package));
    }
    let mut command = Command::new("brew");
    command.arg("install");
    if cask {
        command.arg("--cask");
    }
    command.arg(package);
    command
        .status()
        .with_context(|| format!("install brew package {}", package))?
        .exit_ok("brew install")?;
    Ok(format!("installed brew package {}", package))
}

fn apply_download_file(
    url: &str,
    dest: &str,
    checksum: &crate::automation::task::Checksum,
) -> Result<String> {
    let dest_path = expand_required_path(dest)?;
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create download parent {}", parent.display()))?;
    }
    let temp_path = unique_temp_path(&dest_path)?;
    let status = Command::new("curl")
        .args([
            "-L",
            "--fail",
            "--silent",
            "--show-error",
            "--output",
            &temp_path.to_string_lossy(),
            url,
        ])
        .status()
        .with_context(|| format!("download {}", url))?;
    status.exit_ok("curl download")?;
    if !checksum.sha256.trim().is_empty() {
        let expected = checksum.sha256.trim();
        let actual = sha256_file(&temp_path)?;
        if actual != expected {
            fs::remove_file(&temp_path).ok();
            bail!(
                "checksum mismatch for {}: expected {}, got {}",
                url,
                expected,
                actual
            );
        }
    }
    if dest_path.exists() {
        fs::remove_file(&dest_path)
            .with_context(|| format!("replace existing destination {}", dest_path.display()))?;
    }
    fs::rename(&temp_path, &dest_path)
        .with_context(|| format!("move downloaded file to {}", dest_path.display()))?;
    Ok(format!("downloaded {} to {}", url, dest_path.display()))
}

fn apply_extract_archive(src: &str, dest: &str) -> Result<String> {
    let src_path = expand_required_path(src)?;
    let dest_path = expand_required_path(dest)?;
    if !src_path.exists() {
        bail!("archive source not found: {}", src_path.display());
    }
    if dest_path.exists() && !dest_path.is_dir() {
        bail!(
            "archive destination exists and is not a directory: {}",
            dest_path.display()
        );
    }
    fs::create_dir_all(&dest_path)
        .with_context(|| format!("create archive destination {}", dest_path.display()))?;

    let manifest = Command::new("tar")
        .arg("-tf")
        .arg(&src_path)
        .output()
        .with_context(|| format!("list archive contents {}", src_path.display()))?;
    if !manifest.status.success() {
        bail!("failed to inspect archive {}", src_path.display());
    }
    for line in String::from_utf8_lossy(&manifest.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rel = Path::new(trimmed);
        if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("archive entry {} escapes the destination", trimmed);
        }
        let candidate = dest_path.join(rel);
        if !candidate.starts_with(&dest_path) {
            bail!("archive entry {} escapes the destination", trimmed);
        }
    }

    let status = Command::new("tar")
        .args(["-xf", &src_path.to_string_lossy()])
        .current_dir(&dest_path)
        .status()
        .with_context(|| format!("extract archive {}", src_path.display()))?;
    status.exit_ok("tar extract")?;
    Ok(format!(
        "extracted {} into {}",
        src_path.display(),
        dest_path.display()
    ))
}

fn apply_install_dmg(dmg: &str, app_name: Option<&str>) -> Result<String> {
    let dmg_path = expand_required_path(dmg)?;
    if !dmg_path.exists() {
        bail!("dmg not found: {}", dmg_path.display());
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let install_root = home.join("Applications");
    fs::create_dir_all(&install_root)
        .with_context(|| format!("create install root {}", install_root.display()))?;
    let app_label = app_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            dmg_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("app")
        });
    let install_dir = install_root.join(app_label);
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("create install dir {}", install_dir.display()))?;
    let target_path = install_dir.join(dmg_path.file_name().unwrap_or_default());
    fs::copy(&dmg_path, &target_path).with_context(|| {
        format!(
            "copy dmg {} to {}",
            dmg_path.display(),
            target_path.display()
        )
    })?;
    Ok(format!(
        "staged dmg {} into {}",
        dmg_path.display(),
        target_path.display()
    ))
}

fn apply_install_pkg(pkg: &str, target: Option<&str>) -> Result<String> {
    let pkg_path = expand_required_path(pkg)?;
    if !pkg_path.exists() {
        bail!("pkg not found: {}", pkg_path.display());
    }
    let target_path = if let Some(target) = target {
        expand_required_path(target)?
    } else {
        dirs::home_dir()
            .map(|home| home.join("Library/Packages"))
            .ok_or_else(|| anyhow!("home directory unavailable"))?
    };
    fs::create_dir_all(&target_path)
        .with_context(|| format!("create pkg target {}", target_path.display()))?;
    let destination = target_path.join(pkg_path.file_name().unwrap_or_default());
    fs::copy(&pkg_path, &destination).with_context(|| {
        format!(
            "copy pkg {} to {}",
            pkg_path.display(),
            destination.display()
        )
    })?;
    Ok(format!(
        "staged pkg {} into {}",
        pkg_path.display(),
        destination.display()
    ))
}

fn apply_run_command(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    shell: ShellMode,
) -> Result<String> {
    let mut command = if matches!(shell, ShellMode::Allow) {
        let shell_command = render_shell_command(program, args);
        let mut shell_runner = Command::new("sh");
        shell_runner.arg("-lc").arg(shell_command);
        shell_runner
    } else {
        let mut direct = Command::new(program);
        direct.args(expand_args(args)?);
        direct
    };
    if let Some(cwd) = cwd {
        command.current_dir(expand_required_path(cwd)?);
    }
    for (key, value) in env {
        command.env(key, expand_env_value(value)?);
    }
    command
        .status()
        .with_context(|| format!("run command {}", program))?
        .exit_ok(program)?;
    Ok(format!("ran {}", render_command(program, args, cwd)))
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn unique_temp_path(dest: &Path) -> Result<PathBuf> {
    let mut index = 0u64;
    loop {
        let candidate = if let Some(ext) = dest.extension() {
            let mut path = dest.to_path_buf();
            let new_ext = format!("{}.{}.part", ext.to_string_lossy(), index);
            path.set_extension(new_ext);
            path
        } else {
            dest.with_extension(format!("part.{}", index))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
        index += 1;
    }
}

fn expand_required_path(raw: &str) -> Result<PathBuf> {
    expand_path_template(raw).ok_or_else(|| anyhow!("unexpanded path template {}", raw))
}

fn expand_args(args: &[String]) -> Result<Vec<OsString>> {
    args.iter().map(|arg| expand_arg(arg)).collect()
}

fn expand_arg(arg: &str) -> Result<OsString> {
    if let Some(path) = expand_path_template(arg) {
        return Ok(path.into_os_string());
    }
    Ok(OsString::from(arg))
}

fn expand_env_value(value: &str) -> Result<OsString> {
    if let Some(path) = expand_path_template(value) {
        return Ok(path.into_os_string());
    }
    Ok(OsString::from(value))
}

fn render_command(program: &str, args: &[String], cwd: Option<&str>) -> String {
    let mut rendered = String::from(program);
    for arg in args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    if let Some(cwd) = cwd {
        rendered.push_str(" (cwd ");
        rendered.push_str(cwd);
        rendered.push(')');
    }
    rendered
}

fn render_shell_command(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell_escape(program));
    for arg in args {
        parts.push(shell_escape(arg));
    }
    parts.join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

trait ExitStatusExt {
    fn exit_ok(self, action: &str) -> Result<()>;
}

impl ExitStatusExt for ExitStatus {
    fn exit_ok(self, action: &str) -> Result<()> {
        if self.success() {
            return Ok(());
        }
        match self.code() {
            Some(code) => bail!("{} failed with exit code {}", action, code),
            None => bail!("{} terminated by signal", action),
        }
    }
}

fn is_satisfied(step: &Step, run_command_checks: bool) -> Result<Option<String>> {
    if run_command_checks {
        if let Action::ConfigurePackageRegistryFiles {
            secrets,
            npm,
            nuget,
        } = &step.action
        {
            if let Some(reason) = package_registry::is_satisfied(secrets, npm, nuget)? {
                return Ok(Some(reason));
            }
        }
    }

    let Some(check) = &step.check else {
        return Ok(None);
    };
    if let Some(path) = &check.path_exists {
        let expanded =
            expand_path_template(&path.to_string_lossy()).unwrap_or_else(|| path.clone());
        if expanded.exists() {
            return Ok(Some(format!("path exists: {}", expanded.display())));
        }
    }
    if let Some(cmd) = &check.command_succeeds {
        if cmd.is_empty() || !run_command_checks {
            return Ok(None);
        }
        let status = Command::new(&cmd[0])
            .args(&cmd[1..])
            .status()
            .with_context(|| format!("run satisfaction check for step {}", step.id))?;
        if status.success() {
            return Ok(Some(format!("command succeeded: {}", cmd.join(" "))));
        }
    }
    Ok(None)
}

pub fn extracted_path_is_safe(root: &Path, rel: &Path) -> bool {
    let candidate = root.join(rel);
    stays_under_root(root, &candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::task::{Checksum, Task, TrustRequirement};
    use std::path::PathBuf;

    fn base_task(step: Step) -> Task {
        Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: String::new(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            steps: vec![step],
        }
    }

    #[test]
    fn run_task_plans_by_default() {
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::DownloadFile {
                url: "https://example.com/app.tgz".into(),
                dest: "$HOME/Library/Caches/app.tgz".into(),
                checksum: Checksum {
                    sha256: "abc".into(),
                },
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans.len(), 1);
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(report.plans[0].prerequisites.is_empty());
    }

    #[test]
    fn shell_requires_flag() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunCommand {
                program: "bash".into(),
                args: vec!["-lc".into(), "echo hi".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Allow,
            },
        });
        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("--allow-shell"));
    }

    #[test]
    fn archive_traversal_blocked() {
        let root = PathBuf::from("/tmp/root");
        assert!(extracted_path_is_safe(&root, Path::new("dir/file.txt")));
        assert!(!extracted_path_is_safe(&root, Path::new("../escape.txt")));
    }

    #[test]
    fn download_action_writes_file_and_verifies_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.bin");
        fs::write(&source, b"hello").unwrap();
        let dest = tmp.path().join("downloaded.bin");
        let checksum = crate::automation::task::Checksum {
            sha256: sha256_file(&source).unwrap(),
        };
        let summary = apply_download_file(
            &format!("file://{}", source.display()),
            &dest.to_string_lossy(),
            &checksum,
        )
        .unwrap();
        assert!(dest.exists());
        assert!(summary.contains("downloaded"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[test]
    fn archive_action_extracts_only_safe_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("archive.tar");
        let dest = tmp.path().join("out");
        fs::write(tmp.path().join("payload.txt"), b"payload").unwrap();
        let status = Command::new("tar")
            .args([
                "-cf",
                &archive.to_string_lossy(),
                "-C",
                &tmp.path().to_string_lossy(),
                "payload.txt",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let summary =
            apply_extract_archive(&archive.to_string_lossy(), &dest.to_string_lossy()).unwrap();
        assert!(summary.contains("extracted"));
        assert!(dest.join("payload.txt").exists());
    }

    #[test]
    fn satisfied_step_reports_reason() {
        let task = base_task(Step {
            id: "clone".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                command_succeeds: None,
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::GitClone {
                repo: "https://github.com/example/repo.git".into(),
                dest: "$HOME/Library/Caches/repo".into(),
                branch: None,
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.plans.is_empty());
        match &report.outcomes[0] {
            ActionOutcome::AlreadySatisfied { reason } => {
                assert!(reason.contains("path exists"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn planning_mode_skips_command_satisfaction_checks() {
        let task = base_task(Step {
            id: "brew".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: None,
                command_succeeds: Some(vec!["true".into()]),
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::BrewInstall {
                package: "git".into(),
                cask: false,
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
    }

    #[test]
    fn git_clone_plan_can_require_one_time_git_auth() {
        let task = base_task(Step {
            id: "clone".into(),
            name: String::new(),
            auth: AuthPolicy::GitCredential,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::GitClone {
                repo: "https://github.com/example/repo.git".into(),
                dest: "$HOME/Library/Caches/repo".into(),
                branch: None,
            },
        });
        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans[0].prerequisites.len(), 1);
        assert!(report.plans[0].prerequisites[0].contains("git once"));
    }

    #[test]
    fn elevated_plan_can_require_one_time_sudo_auth() {
        let task = base_task(Step {
            id: "remote-login".into(),
            name: String::new(),
            auth: AuthPolicy::Sudo,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Allow,
            action: Action::RunCommand {
                program: "sudo".into(),
                args: vec!["systemsetup".into(), "-getremotelogin".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                allow_elevation: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.plans[0].prerequisites.len(), 1);
        assert!(report.plans[0].prerequisites[0].contains("sudo once"));
    }

    #[test]
    fn apply_mode_downloads_to_a_local_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("app.tgz");
        fs::write(&source, b"payload").unwrap();
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::DownloadFile {
                url: format!("file://{}", source.display()),
                dest: tmp
                    .path()
                    .join("downloaded.tgz")
                    .to_string_lossy()
                    .into_owned(),
                checksum: Checksum {
                    sha256: sha256_file(&source).unwrap(),
                },
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.errors.is_empty());
    }

    #[test]
    fn planning_mode_keeps_auth_steps_in_order() {
        let task = Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: String::new(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            steps: vec![
                Step {
                    id: "clone".into(),
                    name: String::new(),
                    auth: AuthPolicy::GitCredential,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::GitClone {
                        repo: "https://github.com/example/repo.git".into(),
                        dest: "$HOME/Library/Caches/repo".into(),
                        branch: None,
                    },
                },
                Step {
                    id: "brew".into(),
                    name: String::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::BrewInstall {
                        package: "git".into(),
                        cask: false,
                    },
                },
            ],
        };

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.plans.len(), 2);
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(matches!(report.outcomes[1], ActionOutcome::Planned { .. }));
        assert_eq!(report.steps[0].step_id, "clone");
        assert_eq!(report.steps[1].step_id, "brew");
    }

    #[test]
    fn failed_step_is_reported_in_run_report() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunCommand {
                program: "false".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert_eq!(report.errors.len(), 1);
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
    }

    #[test]
    fn successful_run_command_apply_is_reported() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunCommand {
                program: "true".into(),
                args: vec![],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Forbidden,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(matches!(report.outcomes[0], ActionOutcome::Applied { .. }));
    }

    #[test]
    fn steps_after_failure_are_still_reported() {
        let task = Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: String::new(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            steps: vec![
                Step {
                    id: "fail".into(),
                    name: String::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::RunCommand {
                        program: "false".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                        shell: ShellMode::Forbidden,
                    },
                },
                Step {
                    id: "later".into(),
                    name: String::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::RunCommand {
                        program: "true".into(),
                        args: vec![],
                        cwd: None,
                        env: Default::default(),
                        shell: ShellMode::Forbidden,
                    },
                },
            ],
        };
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert_eq!(report.steps.len(), 2);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.steps[1].status, StepStatus::Skipped));
    }

    #[test]
    fn run_task_rejects_invalid_programmatic_package_registry_action() {
        let task = base_task(Step {
            id: "package-config".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::ConfigurePackageRegistryFiles {
                secrets: crate::automation::task::EncryptedSecretsSpec {
                    profile: "github-packages".into(),
                    username_env: "GITHUB_PACKAGES_USER".into(),
                    token_env: "GITHUB_PACKAGES_TOKEN".into(),
                },
                npm: crate::automation::task::NpmRegistryFileSpec {
                    scope: "@dodopizza".into(),
                    registry: "http://npm.pkg.github.com/".into(),
                },
                nuget: crate::automation::task::NugetRegistryFileSpec {
                    public_source_name: "nuget.org".into(),
                    public_source: "https://api.nuget.org/v3/index.json".into(),
                    source_name: "github".into(),
                    source: "https://nuget.pkg.github.com/dodopizza/index.json".into(),
                    package_patterns: vec!["Dodo.*".into()],
                },
            },
        });

        let err = run_task(&task, &RunOptions::default()).unwrap_err();

        assert!(err.to_string().contains("npm.registry to be an HTTPS URL"));
    }

    #[test]
    fn shell_mode_allow_runs_via_shell() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunCommand {
                program: "sh".into(),
                args: vec!["-lc".into(), ":".into()],
                cwd: None,
                env: Default::default(),
                shell: ShellMode::Allow,
            },
        });
        let report = run_task(
            &task,
            &RunOptions {
                apply: true,
                allow_shell: true,
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(matches!(report.outcomes[0], ActionOutcome::Applied { .. }));
    }
}
