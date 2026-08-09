use crate::automation::task::{
    Action, AppBundleIdentity, AppStoreOperation, AuthPolicy, ElevationPolicy, LicenseMethod,
    LicenseProvider, ReleaseChannel, ShellMode, Step, Task,
};
use crate::rules::expand_path_template;
use crate::safety::{is_safe_rule_root, stays_under_root};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
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
    pub release_channel: Option<ReleaseChannel>,
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
    run_task_with_interactivity(task, opts, terminal_is_interactive())
}

fn run_task_with_interactivity(
    task: &Task,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<RunReport> {
    if opts.release_channel.is_some()
        && !task
            .steps
            .iter()
            .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
    {
        bail!("--channel is only supported by tasks with a bambu-studio-release step");
    }
    // Validate every policy gate before the first applied step so a missing
    // acknowledgement cannot leave a task partially applied.
    for step in &task.steps {
        enforce_step_policy(step, opts, terminal_interactive)?;
    }

    let mut plans = Vec::new();
    let mut outcomes = Vec::new();
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    let mut auth_state = AuthState::default();
    let mut halted = false;

    for step in &task.steps {
        if halted {
            let plan = plan_step(step, opts)?;
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
        let satisfaction = is_satisfied(step, opts.apply)?;
        if let Some(reason) = satisfaction {
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: plan_summary(step, opts)?,
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
        let plan = plan_step(step, opts)?;
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
            if matches!(
                &step.action,
                Action::ActivateLicense(_) | Action::AppStoreInstall(_)
            ) {
                steps[step_idx].status = StepStatus::WaitingForAttention;
                steps[step_idx].logs.push(StepLogEntry {
                    step_id: step.id.clone(),
                    message: match &step.action {
                        Action::ActivateLicense(_) => {
                            "waiting for license activation in the vendor UI".into()
                        }
                        Action::AppStoreInstall(_) => {
                            "waiting for any required App Store authentication".into()
                        }
                        _ => unreachable!(),
                    },
                });
            } else {
                steps[step_idx].status = StepStatus::Running;
            }
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: "running".into(),
            });
            let summary = match apply_step(step, opts) {
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

fn enforce_step_policy(step: &Step, opts: &RunOptions, terminal_interactive: bool) -> Result<()> {
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
    if opts.apply && matches!(&step.action, Action::ActivateLicense(_)) && !terminal_interactive {
        return Err(AutomationError::Message(format!(
            "step {} requires an interactive terminal for vendor UI license activation",
            step.id
        ))
        .into());
    }
    validate_destinations(step)?;
    if opts.apply {
        validate_existing_dmg_install(step)?;
    }
    Ok(())
}

fn validate_existing_dmg_install(step: &Step) -> Result<()> {
    let Action::InstallDmg {
        app_name: Some(app_name),
        target,
        identity: Some(identity),
        ..
    } = &step.action
    else {
        return Ok(());
    };
    let destination = dmg_install_destination(app_name, target.as_deref())?;
    if path_entry_exists(&destination)? {
        verify_app_identity(&destination, identity).with_context(|| {
            format!(
                "existing application does not match the pinned identity and version: {}",
                destination.display()
            )
        })?;
    }
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
        Action::InstallDmg { target, .. } => validate_dmg_target(step, target.as_deref())?,
        _ => {}
    }
    Ok(())
}

fn validate_dmg_target(step: &Step, target: Option<&str>) -> Result<()> {
    let raw_target = target.unwrap_or("$HOME/Applications");
    let target_path = expand_required_path(raw_target)?;
    let home_apps = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Applications");

    if target_path != home_apps {
        bail!(
            "step {} dmg target {} is not allowed; install-dmg is restricted to $HOME/Applications",
            step.id,
            target_path.display()
        );
    }
    if !matches!(step.auth, AuthPolicy::None)
        || !matches!(step.allow_elevation, ElevationPolicy::Forbidden)
    {
        bail!(
            "step {} install-dmg must not request authentication or elevation",
            step.id
        );
    }
    Ok(())
}

fn dmg_install_destination(app_name: &str, target: Option<&str>) -> Result<PathBuf> {
    Ok(expand_required_path(target.unwrap_or("$HOME/Applications"))?.join(app_name))
}

fn parent_or_self(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

fn plan_step(step: &Step, opts: &RunOptions) -> Result<ActionPlan> {
    let prerequisites = prerequisites_for_step(step);
    let summary = plan_summary(step, opts)?;
    Ok(ActionPlan {
        step_id: step.id.clone(),
        step_name: step_name(step),
        summary,
        prerequisites,
    })
}

fn plan_summary(step: &Step, opts: &RunOptions) -> Result<String> {
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
        Action::DownloadFile { url, dest, .. } => {
            format!("download {} to {} with sha256 verification", url, dest)
        }
        Action::ExtractArchive { src, dest } => {
            format!("extract {} into {} with traversal protection", src, dest)
        }
        Action::InstallDmg {
            dmg,
            app_name,
            target,
            identity,
        } => {
            let identity_summary = identity
                .as_ref()
                .map(|identity| {
                    format!(
                        ", require bundle {} version {} signed by team {}",
                        identity.bundle_identifier, identity.version, identity.team_identifier
                    )
                })
                .unwrap_or_default();
            format!(
                "verify and mount {} read-only, validate signature{}, install {} into {}",
                dmg,
                identity_summary,
                app_name.as_deref().unwrap_or("the only .app bundle"),
                target.as_deref().unwrap_or("$HOME/Applications")
            )
        }
        Action::InstallPkg { pkg, target } => format!(
            "validate pkg signature for {} and install to {}",
            pkg,
            target.as_deref().unwrap_or("/")
        ),
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => format!(
            "require macOS {} or newer{}",
            minimum_version,
            if *require_rosetta_on_apple_silicon {
                " and Rosetta on Apple Silicon"
            } else {
                ""
            }
        ),
        Action::AppStoreInstall(action) => format!(
            "mas {} Mac App Store application {}",
            app_store_operation_name(action.operation),
            action.app_id
        ),
        Action::BambuStudioRelease(action) => format!(
            "resolve the latest Bambu Studio {} from the official GitHub releases, compare its version with the signed installed app, and install only when newer",
            release_channel_name(opts.release_channel.unwrap_or(action.channel))
        ),
        Action::ActivateLicense(action) => match (&action.provider, &action.method) {
            (LicenseProvider::LightBurn, LicenseMethod::VendorUi) =>
                "launch LightBurn and wait for user-confirmed activation in its License Page; the license key is entered only in LightBurn"
                    .into(),
        },
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
    if matches!(&step.action, Action::ActivateLicense(_)) {
        prerequisites.push(
            "enter the license key only in the vendor application; ppduster does not read, store, or log it"
                .into(),
        );
    }
    if let Action::AppStoreInstall(action) = &step.action {
        prerequisites.push(
            "sign in to the Mac App Store with the Apple Account that owns the application; authentication stays in Apple's UI"
                .into(),
        );
        if matches!(action.operation, AppStoreOperation::Install) {
            prerequisites.push(
                "the application must already be obtained or purchased; use operation: get for a free app that is not yet associated with the account"
                    .into(),
            );
        }
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
            Command::new("/usr/bin/sudo")
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
    Ok(Command::new("/usr/bin/sudo")
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

fn apply_step(step: &Step, opts: &RunOptions) -> Result<String> {
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
        Action::DownloadFile {
            url,
            dest,
            checksum,
        } => apply_download_file(url, dest, checksum),
        Action::ExtractArchive { src, dest } => apply_extract_archive(src, dest),
        Action::InstallDmg {
            dmg,
            app_name,
            target,
            identity,
        } => apply_install_dmg(
            dmg,
            app_name.as_deref(),
            target.as_deref(),
            identity.as_ref(),
            false,
        ),
        Action::InstallPkg { pkg, target } => apply_install_pkg(pkg, target.as_deref()),
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => apply_macos_requirements(minimum_version, *require_rosetta_on_apple_silicon),
        Action::AppStoreInstall(action) => apply_app_store_install(action.app_id, action.operation),
        Action::BambuStudioRelease(action) => {
            apply_bambu_studio_release(opts.release_channel.unwrap_or(action.channel))
        }
        Action::ActivateLicense(action) => apply_activate_license(action.provider, action.method),
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
    let (temp_path, temp_file) = create_unique_temp_file(&dest_path)?;
    let curl_program = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let status = match Command::new(curl_program)
        .args(["-L", "--fail", "--silent", "--show-error", url])
        .stdout(Stdio::from(temp_file))
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            fs::remove_file(&temp_path).ok();
            return Err(err).with_context(|| format!("download {}", url));
        }
    };
    if !status.success() {
        fs::remove_file(&temp_path).ok();
        status.exit_ok("curl download")?;
    }
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
    if path_entry_exists(&dest_path)? {
        fs::remove_file(&dest_path)
            .with_context(|| format!("replace existing destination {}", dest_path.display()))?;
    }
    fs::rename(&temp_path, &dest_path)
        .with_context(|| format!("move downloaded file to {}", dest_path.display()))?;
    Ok(format!("downloaded {} to {}", url, dest_path.display()))
}

const BAMBU_RELEASES_API: &str =
    "https://api.github.com/repos/bambulab/BambuStudio/releases?per_page=30";
const BAMBU_DOWNLOAD_PREFIX: &str = "https://github.com/bambulab/BambuStudio/releases/download/";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

struct ResolvedBambuRelease {
    tag: String,
    version: String,
    asset_name: String,
    download_url: String,
    sha256: String,
}

fn release_channel_name(channel: ReleaseChannel) -> &'static str {
    match channel {
        ReleaseChannel::Release => "release",
        ReleaseChannel::Beta => "beta",
    }
}

fn fetch_bambu_releases() -> Result<Vec<GithubRelease>> {
    let curl_program = if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let output = Command::new(curl_program)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-filesize",
            "5242880",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            BAMBU_RELEASES_API,
        ])
        .output()
        .context("fetch official Bambu Studio release metadata")?;
    if !output.status.success() {
        bail!(
            "fetch official Bambu Studio release metadata failed: {}",
            command_error_detail(&output)
        );
    }
    serde_json::from_slice(&output.stdout).context("parse Bambu Studio release metadata")
}

fn resolve_bambu_release(
    releases: &[GithubRelease],
    channel: ReleaseChannel,
) -> Result<ResolvedBambuRelease> {
    let release = releases
        .iter()
        .filter(|release| {
            !release.draft
                && match channel {
                    ReleaseChannel::Release => !release.prerelease,
                    ReleaseChannel::Beta => release.prerelease,
                }
        })
        .max_by(|left, right| left.published_at.cmp(&right.published_at))
        .ok_or_else(|| {
            anyhow!(
                "official Bambu Studio repository has no {} release",
                release_channel_name(channel)
            )
        })?;

    let mut assets = release.assets.iter().filter(|asset| {
        asset.name.starts_with("Bambu_Studio_mac-v")
            && asset.name.ends_with(".dmg")
            && !asset.name.contains("pre_release")
    });
    let asset = assets.next().ok_or_else(|| {
        anyhow!(
            "Bambu Studio {} has no unambiguous macOS DMG",
            release.tag_name
        )
    })?;
    if assets.next().is_some() {
        bail!(
            "Bambu Studio {} has multiple matching macOS DMGs; refusing an ambiguous download",
            release.tag_name
        );
    }
    if Path::new(&asset.name).components().count() != 1 {
        bail!("Bambu Studio release contains an unsafe asset name");
    }
    if !asset
        .browser_download_url
        .starts_with(BAMBU_DOWNLOAD_PREFIX)
    {
        bail!("Bambu Studio asset URL is outside the official GitHub repository");
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or_else(|| anyhow!("Bambu Studio macOS asset has no official SHA-256 digest"))?;
    if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("Bambu Studio macOS asset has an invalid SHA-256 digest");
    }
    let version = asset
        .name
        .strip_prefix("Bambu_Studio_mac-v")
        .and_then(|rest| rest.split_once('-').map(|(version, _)| version))
        .ok_or_else(|| anyhow!("cannot derive Bambu Studio version from asset name"))?;
    parse_version(version)?;

    Ok(ResolvedBambuRelease {
        tag: release.tag_name.clone(),
        version: version.to_string(),
        asset_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        sha256: digest.to_ascii_lowercase(),
    })
}

fn apply_bambu_studio_release(channel: ReleaseChannel) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("bambu-studio-release is only supported on macOS");
    }
    let resolved = resolve_bambu_release(&fetch_bambu_releases()?, channel)?;
    let install_root = expand_required_path("$HOME/Applications")?;
    let destination = install_root.join("BambuStudio.app");

    if path_entry_exists(&destination)? {
        let installed_version = verify_bambu_app_and_read_version(&destination)?;
        if compare_versions(&installed_version, &resolved.version)? != Ordering::Less {
            return Ok(format!(
                "Bambu Studio {} is already installed; latest {} is {} ({})",
                installed_version,
                release_channel_name(channel),
                resolved.version,
                resolved.tag
            ));
        }
        let running = Command::new("/usr/bin/pgrep")
            .args(["-x", "BambuStudio"])
            .status()
            .context("check whether Bambu Studio is running")?;
        if running.success() {
            bail!("Bambu Studio is running; quit it before updating");
        }
        if running.code() != Some(1) {
            bail!("could not determine whether Bambu Studio is running");
        }
    }

    let cache_path = dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory unavailable"))?
        .join("Library/Caches/ppduster/downloads")
        .join(&resolved.asset_name);
    let cache = cache_path.to_string_lossy().into_owned();
    apply_download_file(
        &resolved.download_url,
        &cache,
        &crate::automation::task::Checksum {
            sha256: resolved.sha256.clone(),
        },
    )?;
    let identity = AppBundleIdentity {
        bundle_identifier: "com.bambulab.bambu-studio".into(),
        team_identifier: "T3UBR9Y3B2".into(),
        version: resolved.version.clone(),
    };
    apply_install_dmg(
        &cache,
        Some("BambuStudio.app"),
        Some("$HOME/Applications"),
        Some(&identity),
        true,
    )?;
    Ok(format!(
        "installed Bambu Studio {} from latest {} ({})",
        resolved.version,
        release_channel_name(channel),
        resolved.tag
    ))
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

fn apply_install_dmg(
    dmg: &str,
    app_name: Option<&str>,
    target: Option<&str>,
    identity: Option<&AppBundleIdentity>,
    replace_existing: bool,
) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("install-dmg is only supported on macOS");
    }
    let dmg_path = expand_required_path(dmg)?;
    if !dmg_path.is_file() {
        bail!("dmg not found: {}", dmg_path.display());
    }

    verify_dmg(&dmg_path)?;
    let mount = MountedDmg::attach(&dmg_path)?;
    let install_result = (|| {
        let source_app = find_mounted_app(mount.path(), app_name)?;
        verify_installable_app(&source_app, identity)?;

        let install_root = expand_required_path(target.unwrap_or("$HOME/Applications"))?;
        fs::create_dir_all(&install_root)
            .with_context(|| format!("create install root {}", install_root.display()))?;
        require_real_directory(&install_root)?;

        let bundle_name = source_app
            .file_name()
            .ok_or_else(|| anyhow!("mounted app has no bundle name"))?;
        let destination = install_root.join(bundle_name);
        let destination_exists = path_entry_exists(&destination)?;
        if destination_exists && !replace_existing {
            bail!(
                "application already exists: {}; remove it explicitly before reinstalling",
                destination.display()
            );
        }
        if destination_exists {
            let expected = identity.ok_or_else(|| {
                anyhow!("replacing an application requires an exact signed identity")
            })?;
            verify_app_publisher(&destination, expected).with_context(|| {
                format!(
                    "refuse to replace an application from a different publisher: {}",
                    destination.display()
                )
            })?;
        }

        let staging = unique_app_staging_path(&install_root, bundle_name)?;
        if let Err(copy_err) = copy_app_bundle(&source_app, &staging) {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(copy_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(copy_err);
        }
        if let Err(verify_err) = verify_installable_app(&staging, identity) {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(verify_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(verify_err.context(format!("verify staged app {}", staging.display())));
        }
        let commit_result = if destination_exists {
            replace_with_staged_app(&install_root, &staging, &destination, identity)
        } else {
            commit_staged_app(&staging, &destination)
        };
        if let Err(commit_err) = commit_result {
            if let Err(cleanup_err) = remove_staged_app(&install_root, &staging) {
                return Err(commit_err.context(format!(
                    "also failed to remove staging app: {cleanup_err:#}"
                )));
            }
            return Err(commit_err);
        }
        verify_installable_app(&destination, identity)
            .with_context(|| format!("verify installed app {}", destination.display()))?;
        Ok(destination)
    })();

    let detach_result = mount.detach();
    let destination = match (install_result, detach_result) {
        (Ok(destination), Ok(())) => destination,
        (Ok(_), Err(detach_err)) => return Err(detach_err),
        (Err(install_err), Ok(())) => return Err(install_err),
        (Err(install_err), Err(detach_err)) => {
            return Err(install_err.context(format!("also failed to detach dmg: {detach_err:#}")))
        }
    };

    Ok(format!(
        "installed application from {} into {}",
        dmg_path.display(),
        destination.display()
    ))
}

struct MountedDmg {
    mount_point: PathBuf,
    attached: bool,
}

impl MountedDmg {
    fn attach(dmg_path: &Path) -> Result<Self> {
        let mount_point = unique_temp_directory("ppduster-dmg")?;
        let output = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(&mount_point)
            .arg(dmg_path)
            .output()
            .with_context(|| format!("mount dmg {}", dmg_path.display()))?;
        if !output.status.success() {
            fs::remove_dir_all(&mount_point).ok();
            bail!(
                "mount dmg {} failed: {}",
                dmg_path.display(),
                command_error_detail(&output)
            );
        }
        Ok(Self {
            mount_point,
            attached: true,
        })
    }

    fn path(&self) -> &Path {
        &self.mount_point
    }

    fn detach(mut self) -> Result<()> {
        let output = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(&self.mount_point)
            .output()
            .with_context(|| format!("detach dmg at {}", self.mount_point.display()))?;
        if !output.status.success() {
            bail!(
                "detach dmg at {} failed: {}",
                self.mount_point.display(),
                command_error_detail(&output)
            );
        }
        self.attached = false;
        fs::remove_dir_all(&self.mount_point).with_context(|| {
            format!(
                "remove temporary mount point {}",
                self.mount_point.display()
            )
        })?;
        Ok(())
    }
}

impl Drop for MountedDmg {
    fn drop(&mut self) {
        if self.attached {
            let detached = Command::new("/usr/bin/hdiutil")
                .arg("detach")
                .arg(&self.mount_point)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if detached {
                self.attached = false;
                let _ = fs::remove_dir_all(&self.mount_point);
            }
        }
    }
}

fn verify_dmg(dmg_path: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/hdiutil")
        .arg("verify")
        .arg(dmg_path)
        .output()
        .with_context(|| format!("verify dmg {}", dmg_path.display()))?;
    if !output.status.success() {
        bail!(
            "dmg verification failed for {}: {}",
            dmg_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn find_mounted_app(mount_point: &Path, app_name: Option<&str>) -> Result<PathBuf> {
    let source_app = if let Some(app_name) = app_name {
        mount_point.join(app_name)
    } else {
        let mut candidates = fs::read_dir(mount_point)
            .with_context(|| format!("read mounted dmg {}", mount_point.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("app"))
            .collect::<Vec<_>>();
        candidates.sort();
        match candidates.as_slice() {
            [only] => only.clone(),
            [] => bail!("mounted dmg contains no .app bundle"),
            _ => bail!("mounted dmg contains multiple .app bundles; set app_name"),
        }
    };

    let metadata = fs::symlink_metadata(&source_app)
        .with_context(|| format!("inspect app bundle {}", source_app.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "app bundle is not a real directory: {}",
            source_app.display()
        );
    }
    let canonical_mount = mount_point
        .canonicalize()
        .with_context(|| format!("canonicalize mount point {}", mount_point.display()))?;
    let canonical_app = source_app
        .canonicalize()
        .with_context(|| format!("canonicalize app bundle {}", source_app.display()))?;
    if !canonical_app.starts_with(&canonical_mount) {
        bail!("app bundle escapes mounted dmg: {}", source_app.display());
    }
    Ok(source_app)
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("inspect path {}", path.display())),
    }
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "expected a real directory, not a symlink: {}",
            path.display()
        );
    }
    Ok(())
}

fn verify_installable_app(app_path: &Path, identity: Option<&AppBundleIdentity>) -> Result<()> {
    match identity {
        Some(identity) => verify_app_identity(app_path, identity),
        None => verify_app_signature(app_path),
    }
}

fn verify_app_signature(app_path: &Path) -> Result<()> {
    require_real_directory(app_path)?;
    let codesign = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app_path)
        .output()
        .with_context(|| format!("verify code signature for {}", app_path.display()))?;
    if !codesign.status.success() {
        bail!(
            "code signature verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&codesign)
        );
    }

    let assessment = Command::new("/usr/sbin/spctl")
        .args(["--assess", "--type", "execute"])
        .arg(app_path)
        .output()
        .with_context(|| format!("assess app trust for {}", app_path.display()))?;
    if !assessment.status.success() {
        bail!(
            "Gatekeeper assessment failed for {}: {}",
            app_path.display(),
            command_error_detail(&assessment)
        );
    }
    Ok(())
}

fn app_identity_requirement(identity: &AppBundleIdentity) -> String {
    format!(
        "=identifier \"{}\" and anchor apple generic and certificate leaf[subject.OU] = \"{}\" and info[CFBundleShortVersionString] = \"{}\"",
        identity.bundle_identifier, identity.team_identifier, identity.version
    )
}

fn app_identity_verification_arguments(identity: &AppBundleIdentity) -> Vec<OsString> {
    vec![
        "--verify".into(),
        "--deep".into(),
        "--strict".into(),
        "--test-requirement".into(),
        app_identity_requirement(identity).into(),
    ]
}

fn verify_app_identity(app_path: &Path, identity: &AppBundleIdentity) -> Result<()> {
    verify_app_signature(app_path)?;
    let arguments = app_identity_verification_arguments(identity);
    let output = Command::new("/usr/bin/codesign")
        .args(arguments)
        .arg(app_path)
        .output()
        .with_context(|| format!("verify app identity for {}", app_path.display()))?;
    if !output.status.success() {
        bail!(
            "app identity verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn app_publisher_requirement(identity: &AppBundleIdentity) -> String {
    format!(
        "=identifier \"{}\" and anchor apple generic and certificate leaf[subject.OU] = \"{}\"",
        identity.bundle_identifier, identity.team_identifier
    )
}

fn verify_app_publisher(app_path: &Path, identity: &AppBundleIdentity) -> Result<()> {
    verify_app_signature(app_path)?;
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "--test-requirement"])
        .arg(app_publisher_requirement(identity))
        .arg(app_path)
        .output()
        .with_context(|| format!("verify app publisher for {}", app_path.display()))?;
    if !output.status.success() {
        bail!(
            "app publisher verification failed for {}: {}",
            app_path.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn read_app_version(app_path: &Path) -> Result<String> {
    let info_plist = app_path.join("Contents/Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(&info_plist)
        .output()
        .with_context(|| format!("read app version from {}", info_plist.display()))?;
    if !output.status.success() {
        bail!(
            "read app version from {} failed: {}",
            info_plist.display(),
            command_error_detail(&output)
        );
    }
    let version = String::from_utf8(output.stdout).context("app version is not UTF-8")?;
    let version = version.trim().to_string();
    parse_version(&version)?;
    Ok(version)
}

fn verify_bambu_app_and_read_version(app_path: &Path) -> Result<String> {
    let version = read_app_version(app_path)?;
    let identity = AppBundleIdentity {
        bundle_identifier: "com.bambulab.bambu-studio".into(),
        team_identifier: "T3UBR9Y3B2".into(),
        version: version.clone(),
    };
    verify_app_identity(app_path, &identity)?;
    Ok(version)
}

fn copy_app_bundle(source: &Path, destination: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/ditto")
        .arg(source)
        .arg(destination)
        .output()
        .with_context(|| format!("copy app bundle into {}", destination.display()))?;
    if !output.status.success() {
        bail!(
            "copy app bundle into {} failed: {}",
            destination.display(),
            command_error_detail(&output)
        );
    }
    Ok(())
}

fn unique_app_staging_path(install_root: &Path, bundle_name: &std::ffi::OsStr) -> Result<PathBuf> {
    let bundle_name = bundle_name.to_string_lossy();
    for index in 0..1_000u32 {
        let candidate = install_root.join(format!(
            ".{bundle_name}.ppduster-{}-{index}.app",
            std::process::id()
        ));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!("could not allocate an application staging path")
}

fn commit_staged_app(staging: &Path, destination: &Path) -> Result<()> {
    if path_entry_exists(destination)? {
        bail!(
            "application appeared while installing: {}; refusing to overwrite it",
            destination.display()
        );
    }

    fs::rename(staging, destination).with_context(|| {
        format!(
            "move staged app {} into {}",
            staging.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn replace_with_staged_app(
    install_root: &Path,
    staging: &Path,
    destination: &Path,
    identity: Option<&AppBundleIdentity>,
) -> Result<()> {
    let bundle_name = destination
        .file_name()
        .ok_or_else(|| anyhow!("application destination has no bundle name"))?;
    let backup = unique_app_backup_path(install_root, bundle_name)?;
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "move existing app {} to rollback location",
            destination.display()
        )
    })?;
    if let Err(commit_err) = fs::rename(staging, destination) {
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "restore {} after update commit failed: {commit_err}",
                destination.display()
            )
        })?;
        return Err(commit_err).with_context(|| format!("replace app {}", destination.display()));
    }
    if let Err(verify_err) = verify_installable_app(destination, identity) {
        remove_replacement_app(install_root, destination)?;
        fs::rename(&backup, destination).with_context(|| {
            format!(
                "restore {} after installed app verification failed",
                destination.display()
            )
        })?;
        return Err(verify_err).context("verify replacement app");
    }
    remove_backup_app(install_root, &backup)?;
    Ok(())
}

fn unique_app_backup_path(install_root: &Path, bundle_name: &std::ffi::OsStr) -> Result<PathBuf> {
    let bundle_name = bundle_name.to_string_lossy();
    for index in 0..1_000u32 {
        let candidate = install_root.join(format!(
            ".{bundle_name}.ppduster-backup-{}-{index}.app",
            std::process::id()
        ));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    bail!("could not allocate an application rollback path")
}

fn remove_backup_app(install_root: &Path, backup: &Path) -> Result<()> {
    let file_name = backup
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid backup app path {}", backup.display()))?;
    if backup.parent() != Some(install_root)
        || !file_name.starts_with('.')
        || !file_name.contains(".app.ppduster-backup-")
        || !file_name.ends_with(".app")
    {
        bail!(
            "refusing to remove unexpected backup path {}",
            backup.display()
        );
    }
    fs::remove_dir_all(backup).with_context(|| format!("remove rollback app {}", backup.display()))
}

fn remove_replacement_app(install_root: &Path, destination: &Path) -> Result<()> {
    if destination.parent() != Some(install_root)
        || destination.file_name().and_then(|name| name.to_str()) != Some("BambuStudio.app")
    {
        bail!(
            "refusing to remove unexpected replacement path {}",
            destination.display()
        );
    }
    fs::remove_dir_all(destination)
        .with_context(|| format!("remove failed replacement {}", destination.display()))
}

fn remove_staged_app(install_root: &Path, staging: &Path) -> Result<()> {
    let file_name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid staging app path {}", staging.display()))?;
    if staging.parent() != Some(install_root)
        || !file_name.starts_with('.')
        || !file_name.contains(".app.ppduster-")
        || !file_name.ends_with(".app")
    {
        bail!(
            "refusing to remove unexpected staging path {}",
            staging.display()
        );
    }
    if !path_entry_exists(staging)? {
        return Ok(());
    }

    fs::remove_dir_all(staging)
        .with_context(|| format!("remove staged app {}", staging.display()))?;
    Ok(())
}

fn command_error_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_string()
    }
}

fn unique_temp_directory(prefix: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir();
    for index in 0..1_000u32 {
        let candidate = root.join(format!("{prefix}-{}-{index}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("create mount point {}", candidate.display()))
            }
        }
    }
    bail!("could not allocate a temporary dmg mount point")
}

fn apply_macos_requirements(
    minimum_version: &str,
    require_rosetta_on_apple_silicon: bool,
) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("macos-requirements is only supported on macOS");
    }

    let version_output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .context("read macOS version")?;
    if !version_output.status.success() {
        bail!(
            "read macOS version failed: {}",
            command_error_detail(&version_output)
        );
    }
    let current_version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_owned();
    if !version_at_least(&current_version, minimum_version)? {
        bail!(
            "macOS {} is unsupported; this task requires macOS {} or newer",
            current_version,
            minimum_version
        );
    }

    let mut rosetta_checked = false;
    if require_rosetta_on_apple_silicon {
        let architecture_output = Command::new("/usr/bin/uname")
            .arg("-m")
            .output()
            .context("read Mac architecture")?;
        if !architecture_output.status.success() {
            bail!(
                "read Mac architecture failed: {}",
                command_error_detail(&architecture_output)
            );
        }
        let architecture = String::from_utf8_lossy(&architecture_output.stdout);
        if architecture.trim() == "arm64" {
            rosetta_checked = true;
            let rosetta = Command::new("/usr/sbin/pkgutil")
                .args(["--pkg-info", "com.apple.pkg.RosettaUpdateAuto"])
                .output()
                .context("check Rosetta package receipt")?;
            if !rosetta.status.success() {
                bail!(
                    "Rosetta is required on Apple Silicon but is not installed; install it with Apple's softwareupdate tool before retrying"
                );
            }
        }
    }

    Ok(format!(
        "macOS {} satisfies minimum {}{}",
        current_version,
        minimum_version,
        if rosetta_checked {
            "; Rosetta is installed"
        } else {
            ""
        }
    ))
}

fn version_at_least(current: &str, minimum: &str) -> Result<bool> {
    Ok(compare_versions(current, minimum)? != Ordering::Less)
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    let component_count = left.len().max(right.len());
    for index in 0..component_count {
        let left_component = *left.get(index).unwrap_or(&0);
        let right_component = *right.get(index).unwrap_or(&0);
        match left_component.cmp(&right_component) {
            Ordering::Equal => {}
            ordering => return Ok(ordering),
        }
    }
    Ok(Ordering::Equal)
}

fn parse_version(value: &str) -> Result<Vec<u64>> {
    if value.trim().is_empty() {
        bail!("version must not be empty");
    }
    value
        .trim()
        .split('.')
        .map(|component| {
            component
                .parse::<u64>()
                .with_context(|| format!("invalid version component {component:?} in {value:?}"))
        })
        .collect()
}

fn app_store_operation_name(operation: AppStoreOperation) -> &'static str {
    match operation {
        AppStoreOperation::Install => "install",
        AppStoreOperation::Get => "get",
    }
}

fn app_store_command_arguments(app_id: u64, operation: AppStoreOperation) -> Vec<OsString> {
    vec![
        app_store_operation_name(operation).into(),
        app_id.to_string().into(),
    ]
}

fn resolve_mas_binary() -> Result<PathBuf> {
    let candidates = ["/opt/homebrew/bin/mas", "/usr/local/bin/mas"];
    let allowed_roots = ["/opt/homebrew/Cellar/mas", "/usr/local/Cellar/mas"];
    for candidate in candidates {
        let path = Path::new(candidate);
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if canonical.is_file()
            && allowed_roots
                .iter()
                .any(|root| canonical.starts_with(Path::new(root)))
        {
            return Ok(canonical);
        }
    }
    bail!(
        "mas is unavailable from a standard Homebrew installation; run the bundled app-store-bootstrap task first"
    )
}

fn mas_list_contains_app(output: &str, app_id: u64) -> bool {
    let expected = app_id.to_string();
    output.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|value| value == expected)
    })
}

fn app_store_app_is_installed(mas: &Path, app_id: u64) -> Result<bool> {
    let output = Command::new(mas)
        .args(["list", &app_id.to_string()])
        .output()
        .with_context(|| format!("check Mac App Store application {}", app_id))?;
    if !output.status.success() {
        bail!(
            "mas list failed while checking application {}: {}",
            app_id,
            command_error_detail(&output)
        );
    }
    Ok(mas_list_contains_app(
        &String::from_utf8_lossy(&output.stdout),
        app_id,
    ))
}

fn apply_app_store_install(app_id: u64, operation: AppStoreOperation) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("app-store-install is only supported on macOS");
    }
    let mas = resolve_mas_binary()?;
    if app_store_app_is_installed(&mas, app_id)? {
        return Ok(format!(
            "Mac App Store application {} is already installed",
            app_id
        ));
    }
    let arguments = app_store_command_arguments(app_id, operation);
    let output = Command::new(&mas)
        .args(arguments)
        .output()
        .with_context(|| {
            format!(
                "mas {} Mac App Store application {}",
                app_store_operation_name(operation),
                app_id
            )
        })?;
    if !output.status.success() {
        bail!(
            "mas {} failed for Mac App Store application {}: {}",
            app_store_operation_name(operation),
            app_id,
            command_error_detail(&output)
        );
    }
    Ok(format!(
        "mas {} completed for Mac App Store application {}",
        app_store_operation_name(operation),
        app_id
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

fn apply_activate_license(provider: LicenseProvider, method: LicenseMethod) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("LightBurn vendor UI activation is only supported on macOS");
    }
    let interactive = terminal_is_interactive();
    apply_activate_license_with(
        provider,
        method,
        interactive,
        launch_license_ui,
        prompt_activation_confirmation,
    )
}

fn terminal_is_interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn apply_activate_license_with<Launch, Confirm>(
    provider: LicenseProvider,
    method: LicenseMethod,
    interactive: bool,
    mut launch: Launch,
    mut confirm: Confirm,
) -> Result<String>
where
    Launch: FnMut(LicenseProvider) -> Result<()>,
    Confirm: FnMut(&str) -> Result<bool>,
{
    if !interactive {
        bail!(
            "license activation requires an interactive terminal; the key must be entered directly in the vendor UI"
        );
    }

    match (provider, method) {
        (LicenseProvider::LightBurn, LicenseMethod::VendorUi) => {
            launch(provider)?;
            eprintln!(
                "LightBurn is open. Enter the license key in its License Page (or Help -> License Management), then activate it."
            );
            if !confirm(
                "Type ACTIVATED here only after LightBurn reports a successful activation: ",
            )? {
                bail!("LightBurn activation was not confirmed; expected ACTIVATED");
            }
            Ok(
                "user confirmed LightBurn activation in the vendor UI; ppduster did not read or store the license key"
                    .into(),
            )
        }
    }
}

fn launch_license_ui(provider: LicenseProvider) -> Result<()> {
    let app_path = license_application_path(provider)?;
    match provider {
        LicenseProvider::LightBurn => verify_lightburn_identity(&app_path)?,
    }
    require_license_application_stopped(provider)?;
    let arguments = license_launch_arguments(&app_path);
    Command::new("/usr/bin/open")
        .args(arguments)
        .status()
        .with_context(|| format!("open {} license UI", app_path.display()))?
        .exit_ok(&format!("open {}", app_path.display()))?;
    Ok(())
}

fn license_application_path(provider: LicenseProvider) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    match provider {
        LicenseProvider::LightBurn => Ok(home.join("Applications/LightBurn.app")),
    }
}

fn require_license_application_stopped(provider: LicenseProvider) -> Result<()> {
    let process_name = match provider {
        LicenseProvider::LightBurn => "LightBurn",
    };
    let status = Command::new("/usr/bin/pgrep")
        .args(["-x", process_name])
        .status()
        .with_context(|| format!("check for running {} processes", process_name))?;
    match status.code() {
        Some(1) => Ok(()),
        Some(0) => bail!(
            "{} is already running; quit every instance and rerun so ppduster can open the verified app bundle",
            process_name
        ),
        Some(code) => bail!("checking for running {} failed with exit code {}", process_name, code),
        None => bail!("checking for running {} terminated by signal", process_name),
    }
}

fn license_launch_arguments(app_path: &Path) -> Vec<OsString> {
    vec!["-n".into(), app_path.as_os_str().to_owned()]
}

const LIGHTBURN_BUNDLE_IDENTIFIER: &str = "com.LightBurnSoftware.LightBurn";
const LIGHTBURN_TEAM_IDENTIFIER: &str = "UWZQ3LL82C";
const LIGHTBURN_VERSION: &str = "2.1.03";

fn verify_lightburn_identity(app_path: &Path) -> Result<()> {
    let identity = AppBundleIdentity {
        bundle_identifier: LIGHTBURN_BUNDLE_IDENTIFIER.into(),
        team_identifier: LIGHTBURN_TEAM_IDENTIFIER.into(),
        version: LIGHTBURN_VERSION.into(),
    };
    verify_app_identity(app_path, &identity).with_context(|| {
        format!(
            "refuse to open untrusted LightBurn app at {}",
            app_path.display()
        )
    })
}

fn prompt_activation_confirmation(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr()
        .flush()
        .context("flush activation confirmation prompt")?;
    let mut line = String::new();
    let bytes = io::stdin()
        .read_line(&mut line)
        .context("read activation confirmation")?;
    if bytes == 0 {
        bail!("activation confirmation ended before ACTIVATED was entered");
    }
    Ok(line.trim() == "ACTIVATED")
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
        let mut shell_runner = Command::new("/bin/sh");
        shell_runner.arg("-lc").arg(shell_command);
        shell_runner
    } else {
        let trusted_program = if program == "sudo" {
            "/usr/bin/sudo"
        } else {
            program
        };
        let mut direct = Command::new(trusted_program);
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
    let mut file =
        fs::File::open(path).with_context(|| format!("open {} for checksum", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("read {} for checksum", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn create_unique_temp_file(dest: &Path) -> Result<(PathBuf, fs::File)> {
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
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("create temporary download {}", candidate.display()))
            }
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
        if let Action::AppStoreInstall(action) = &step.action {
            let mas = resolve_mas_binary()?;
            if app_store_app_is_installed(&mas, action.app_id)? {
                return Ok(Some(format!(
                    "Mac App Store application {} is already installed",
                    action.app_id
                )));
            }
        }
        if let Action::InstallDmg {
            app_name: Some(app_name),
            target,
            identity: Some(identity),
            ..
        } = &step.action
        {
            let destination = dmg_install_destination(app_name, target.as_deref())?;
            if path_entry_exists(&destination)? {
                verify_app_identity(&destination, identity)?;
                return Ok(Some(format!(
                    "verified {} version {} signed by team {} at {}",
                    identity.bundle_identifier,
                    identity.version,
                    identity.team_identifier,
                    destination.display()
                )));
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
    use crate::automation::task::{
        ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, Checksum, Task,
        TrustRequirement,
    };
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

    #[test]
    fn lightburn_activation_refuses_non_interactive_runs_before_launch() {
        let launched = std::cell::Cell::new(false);
        let err = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            false,
            |_| {
                launched.set(true);
                Ok(())
            },
            |_| Ok(true),
        )
        .unwrap_err();

        assert!(!launched.get());
        assert!(err.to_string().contains("interactive terminal"));
    }

    #[test]
    fn lightburn_activation_launches_only_vendor_ui_and_uses_nonsecret_confirmation() {
        let launched = std::cell::RefCell::new(Vec::new());
        let prompts = std::cell::RefCell::new(Vec::new());
        let summary = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            true,
            |provider| {
                launched.borrow_mut().push(provider);
                Ok(())
            },
            |prompt| {
                prompts.borrow_mut().push(prompt.to_owned());
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(launched.into_inner(), vec![LicenseProvider::LightBurn]);
        let app_path = license_application_path(LicenseProvider::LightBurn).unwrap();
        assert_eq!(
            app_path,
            dirs::home_dir().unwrap().join("Applications/LightBurn.app")
        );
        let launch_arguments = license_launch_arguments(&app_path);
        assert_eq!(launch_arguments[0], "-n");
        assert_eq!(launch_arguments[1], app_path.as_os_str());
        let prompts = prompts.into_inner();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("ACTIVATED"));
        assert!(summary.contains("did not read or store"));
    }

    #[test]
    fn lightburn_activation_requires_explicit_confirmation() {
        let err = apply_activate_license_with(
            LicenseProvider::LightBurn,
            LicenseMethod::VendorUi,
            true,
            |_| Ok(()),
            |_| Ok(false),
        )
        .unwrap_err();

        assert!(err.to_string().contains("expected ACTIVATED"));
    }

    #[test]
    fn non_interactive_license_preflight_runs_before_download() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.dmg");
        let destination = tmp.path().join("downloaded.dmg");
        fs::write(&source, b"not-reached").unwrap();
        let task = Task {
            id: "lightburn-preflight".into(),
            name: "LightBurn preflight".into(),
            description: String::new(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            steps: vec![
                Step {
                    id: "download".into(),
                    name: String::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::DownloadFile {
                        url: format!("file://{}", source.display()),
                        dest: destination.to_string_lossy().into_owned(),
                        checksum: Checksum {
                            sha256: sha256_file(&source).unwrap(),
                        },
                    },
                },
                Step {
                    id: "activate".into(),
                    name: String::new(),
                    auth: AuthPolicy::None,
                    check: None,
                    dangerous: false,
                    allow_elevation: ElevationPolicy::Forbidden,
                    action: Action::ActivateLicense(ActivateLicenseAction {
                        provider: LicenseProvider::LightBurn,
                        method: LicenseMethod::VendorUi,
                    }),
                },
            ],
        };

        let err = run_task_with_interactivity(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("interactive terminal"));
        assert!(!destination.exists());
    }

    #[test]
    fn macos_version_comparison_handles_missing_components() {
        assert!(version_at_least("26.6", "12.0").unwrap());
        assert!(version_at_least("12", "12.0").unwrap());
        assert!(version_at_least("12.0.1", "12").unwrap());
        assert!(!version_at_least("11.7.10", "12.0").unwrap());
    }

    #[test]
    fn app_identity_uses_test_requirement_with_signed_version() {
        let identity = AppBundleIdentity {
            bundle_identifier: "com.LightBurnSoftware.LightBurn".into(),
            team_identifier: "UWZQ3LL82C".into(),
            version: "2.1.03".into(),
        };
        let arguments = app_identity_verification_arguments(&identity);
        let arguments = arguments
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(arguments.iter().any(|arg| arg == "--test-requirement"));
        assert!(!arguments.iter().any(|arg| arg == "--requirement"));
        let requirement = arguments.last().unwrap();
        assert!(requirement.contains("identifier \"com.LightBurnSoftware.LightBurn\""));
        assert!(requirement.contains("subject.OU] = \"UWZQ3LL82C\""));
        assert!(requirement.contains("CFBundleShortVersionString] = \"2.1.03\""));
    }

    #[test]
    fn app_store_install_plan_is_typed_and_reports_prerequisites() {
        let task = base_task(Step {
            id: "install-xcode".into(),
            name: String::new(),
            auth: AuthPolicy::Sudo,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Allow,
            action: Action::AppStoreInstall(AppStoreInstallAction {
                app_id: 497_799_835,
                operation: AppStoreOperation::Install,
            }),
        });

        let report = run_task(
            &task,
            &RunOptions {
                allow_elevation: true,
                ..RunOptions::default()
            },
        )
        .unwrap();

        assert!(report.plans[0]
            .summary
            .contains("mas install Mac App Store application 497799835"));
        assert!(report.plans[0]
            .prerequisites
            .iter()
            .any(|item| item.contains("Mac App Store")));
        assert!(report.plans[0]
            .prerequisites
            .iter()
            .any(|item| item.contains("operation: get")));
    }

    #[test]
    fn mas_list_parser_and_command_arguments_use_exact_app_id() {
        assert!(mas_list_contains_app(
            "497799835 Xcode (26.4)\n",
            497_799_835
        ));
        assert!(!mas_list_contains_app(
            "497799835 Xcode (26.4)\n",
            409_183_694
        ));
        assert_eq!(
            app_store_command_arguments(497_799_835, AppStoreOperation::Get),
            vec![OsString::from("get"), OsString::from("497799835")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_root_must_not_be_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-applications");
        let link = tmp.path().join("Applications");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = require_real_directory(&link).unwrap_err();
        assert!(err.to_string().contains("not a symlink"));
    }

    #[test]
    fn system_app_dmg_install_is_rejected() {
        let task = base_task(Step {
            id: "install".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::InstallDmg {
                dmg: "$HOME/Library/Caches/app.dmg".into(),
                app_name: Some("Example.app".into()),
                target: Some("/Applications".into()),
                identity: None,
            },
        });

        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("restricted to $HOME/Applications"));
    }

    #[test]
    fn user_app_dmg_install_plans_without_elevation() {
        let task = base_task(Step {
            id: "install".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::InstallDmg {
                dmg: "$HOME/Library/Caches/app.dmg".into(),
                app_name: Some("Example.app".into()),
                target: Some("$HOME/Applications".into()),
                identity: None,
            },
        });

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(report.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(report.plans[0].summary.contains("$HOME/Applications"));
        assert!(report.plans[0].prerequisites.is_empty());
    }

    fn github_release(
        tag: &str,
        prerelease: bool,
        published_at: &str,
        asset_name: &str,
        digest: &str,
    ) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            published_at: published_at.into(),
            assets: vec![GithubAsset {
                name: asset_name.into(),
                browser_download_url: format!("{BAMBU_DOWNLOAD_PREFIX}{tag}/{asset_name}"),
                digest: Some(format!("sha256:{digest}")),
            }],
        }
    }

    #[test]
    fn bambu_release_resolver_selects_stable_or_beta_and_asset_version() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let stable = github_release(
            "v02.07.01.62",
            false,
            "2026-06-16T00:00:00Z",
            "Bambu_Studio_mac-v02.07.01.62-20260616.dmg",
            digest,
        );
        let beta = github_release(
            "v02.08.01.55",
            true,
            "2026-07-14T00:00:00Z",
            "Bambu_Studio_mac-v02.08.01.55-20260714.dmg",
            digest,
        );
        let releases = vec![stable, beta];

        let resolved_stable = resolve_bambu_release(&releases, ReleaseChannel::Release).unwrap();
        let resolved_beta = resolve_bambu_release(&releases, ReleaseChannel::Beta).unwrap();
        assert_eq!(resolved_stable.version, "02.07.01.62");
        assert_eq!(resolved_beta.version, "02.08.01.55");
    }

    #[test]
    fn bambu_plan_honors_channel_override_without_network() {
        let task = base_task(Step {
            id: "bambu".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::BambuStudioRelease(crate::automation::task::BambuStudioReleaseAction {
                channel: ReleaseChannel::Release,
            }),
        });
        let report = run_task(
            &task,
            &RunOptions {
                release_channel: Some(ReleaseChannel::Beta),
                ..RunOptions::default()
            },
        )
        .unwrap();
        assert!(report.plans[0].summary.contains("latest Bambu Studio beta"));
    }

    #[test]
    fn numeric_version_comparison_prevents_downgrades() {
        assert_eq!(
            compare_versions("02.08.01.55", "02.07.01.62").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("02.07.01.62", "2.7.1.62").unwrap(),
            Ordering::Equal
        );
    }
}
