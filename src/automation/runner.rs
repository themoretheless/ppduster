use crate::automation::task::{
    Action, AppBundleIdentity, AppStoreOperation, ArchiveFormat, AuthPolicy, ElevationPolicy,
    InspectPathAction, LicenseMethod, LicenseProvider, PathExpectation, PathKind, ReleaseChannel,
    ScriptInterpreter, ShellMode, Step, Task,
};
use crate::rules::expand_path_template;
use crate::safety::{is_safe_rule_root, stays_under_root};
use crate::{app_store, app_store_installer};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::IsTerminal;
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use thiserror::Error;
use walkdir::WalkDir;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<StepOutput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum StepOutput {
    PathMetadata(PathMetadataOutput),
}

#[derive(Debug, Clone, Serialize)]
pub struct PathMetadataOutput {
    pub path: PathBuf,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PathKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionOutcome {
    Planned { action: ActionPlan },
    AlreadySatisfied { reason: String },
    Observed { summary: String },
    Applied { summary: String },
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub task_name: String,
    pub task_description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<String>,
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

#[derive(Debug)]
enum ApplyStepResult {
    Applied(String),
    AlreadySatisfied(String),
}

pub fn run_task(task: &Task, opts: &RunOptions) -> Result<RunReport> {
    run_task_with_interactivity(task, opts, terminal_is_interactive())
}

fn run_task_with_interactivity(
    task: &Task,
    opts: &RunOptions,
    terminal_interactive: bool,
) -> Result<RunReport> {
    task.validate().map_err(anyhow::Error::msg)?;
    if task.steps.is_empty() {
        if task.is_template() {
            bail!(
                "task template {} must be resolved through TaskPack before execution",
                task.id
            );
        }
        bail!("task {} has no executable steps", task.id);
    }
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
                output: None,
            });
            continue;
        }
        // Path inspection is deliberately read-only, so it runs during both a
        // normal dry-run and an applied run. This makes existence, size, and
        // timestamps observable without requiring `--yes`.
        if let Action::InspectPath(action) = &step.action {
            let prerequisites = prerequisites_for_step(step);
            let planned_summary = describe_step(step, opts)?;
            match inspect_path(action) {
                Ok(metadata) => {
                    let summary = summarize_path_metadata(&metadata);
                    let output = Some(StepOutput::PathMetadata(metadata.clone()));
                    let expectation = action
                        .expect
                        .as_ref()
                        .map(|expectation| verify_path_expectation(expectation, &metadata))
                        .transpose();
                    match expectation {
                        Ok(_) => {
                            steps.push(StepReport {
                                step_id: step.id.clone(),
                                step_name: step_name(step),
                                summary: summary.clone(),
                                status: StepStatus::Satisfied,
                                prerequisites,
                                logs: vec![StepLogEntry {
                                    step_id: step.id.clone(),
                                    message: summary.clone(),
                                }],
                                output,
                            });
                            outcomes.push(ActionOutcome::Observed { summary });
                        }
                        Err(error) => {
                            let message =
                                format!("step {} path expectation failed: {error}", step.id);
                            steps.push(StepReport {
                                step_id: step.id.clone(),
                                step_name: step_name(step),
                                summary,
                                status: StepStatus::Failed,
                                prerequisites,
                                logs: vec![StepLogEntry {
                                    step_id: step.id.clone(),
                                    message: format!("failed: {message}"),
                                }],
                                output,
                            });
                            errors.push(message);
                            outcomes.push(ActionOutcome::Blocked);
                            halted = true;
                        }
                    }
                }
                Err(error) => {
                    let message = format!("step {} path inspection failed: {error:#}", step.id);
                    steps.push(StepReport {
                        step_id: step.id.clone(),
                        step_name: step_name(step),
                        summary: planned_summary,
                        status: StepStatus::Failed,
                        prerequisites,
                        logs: vec![StepLogEntry {
                            step_id: step.id.clone(),
                            message: format!("failed: {message}"),
                        }],
                        output: None,
                    });
                    errors.push(message);
                    outcomes.push(ActionOutcome::Blocked);
                    halted = true;
                }
            }
            continue;
        }
        let satisfaction = is_satisfied(step, opts.apply)?;
        if let Some(reason) = satisfaction {
            steps.push(StepReport {
                step_id: step.id.clone(),
                step_name: step_name(step),
                summary: describe_step(step, opts)?,
                status: StepStatus::Satisfied,
                prerequisites: prerequisites_for_step(step),
                logs: vec![StepLogEntry {
                    step_id: step.id.clone(),
                    message: reason.clone(),
                }],
                output: None,
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
            output: None,
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
            let result = match apply_step(step, opts) {
                Ok(result) => result,
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
            let (status, summary, applied) = match result {
                ApplyStepResult::Applied(summary) => (StepStatus::Applied, summary, true),
                ApplyStepResult::AlreadySatisfied(summary) => {
                    (StepStatus::Satisfied, summary, false)
                }
            };
            steps[step_idx].status = status;
            steps[step_idx].summary = summary.clone();
            steps[step_idx].logs.push(StepLogEntry {
                step_id: step.id.clone(),
                message: summary.clone(),
            });
            outcomes.push(if applied {
                ActionOutcome::Applied { summary }
            } else {
                ActionOutcome::AlreadySatisfied { reason: summary }
            });
            continue;
        }
        outcomes.push(ActionOutcome::Planned { action: plan });
    }

    Ok(RunReport {
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        task_description: task.description.clone(),
        scenarios: task.included_scenarios().to_vec(),
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
    let requires_shell_permission = matches!(
        &step.action,
        Action::RunCommand {
            shell: ShellMode::Allow,
            ..
        } | Action::RunScript { .. }
    );
    if requires_shell_permission && !opts.allow_shell {
        return Err(
            AutomationError::Message(format!("step {} requires --allow-shell", step.id)).into(),
        );
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
        Action::CreateDirectory(action) => {
            validate_create_directory_path(&action.path).with_context(|| {
                format!(
                    "step {} directory destination {} blocked by safety",
                    step.id, action.path
                )
            })?;
        }
        Action::InspectPath(action) => {
            validate_declared_path(&action.path).with_context(|| {
                format!("step {} inspect path {} is invalid", step.id, action.path)
            })?;
        }
        Action::GitClone { dest, .. } => {
            let Some(path) = expand_path_template(dest) else {
                bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            validate_resolved_git_destination(&path).with_context(|| {
                format!(
                    "step {} git destination {} blocked by safety",
                    step.id,
                    path.display()
                )
            })?;
        }
        Action::DownloadFile { dest, .. } | Action::ExtractArchive { dest, .. } => {
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

fn validate_declared_path(raw: &str) -> Result<PathBuf> {
    let path = expand_required_path(raw)?;
    if !path.is_absolute() {
        bail!(
            "path must be absolute after template expansion: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("path must not contain '..': {}", path.display());
    }
    lexical_absolute_path(&path)
}

fn validate_create_directory_path(raw: &str) -> Result<PathBuf> {
    let path = validate_declared_path(raw)?;
    let resolved = resolve_through_existing_ancestor(&path)?;
    if !is_safe_rule_root(&resolved) {
        bail!("resolved directory {} is not safe", resolved.display());
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "directory destination must not be a symlink: {}",
                path.display()
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!(
                "directory destination is not a directory: {}",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect directory destination {}", path.display()))
        }
    }
    Ok(path)
}

/// Resolve the longest existing prefix before evaluating the safety policy.
/// This catches destinations redirected through an existing symlink without
/// rejecting normal platform aliases such as macOS `/var` -> `/private/var`.
fn resolve_through_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut deepest_existing = path.to_path_buf();
    let metadata = loop {
        match fs::symlink_metadata(&deepest_existing) {
            Ok(metadata) => break metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !deepest_existing.pop() {
                    return Err(error)
                        .with_context(|| format!("resolve destination {}", path.display()));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect destination ancestor {}",
                        deepest_existing.display()
                    )
                })
            }
        }
    };
    let suffix = path
        .strip_prefix(&deepest_existing)
        .with_context(|| format!("resolve destination suffix for {}", path.display()))?;
    if !suffix.as_os_str().is_empty() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "destination ancestor is not a directory: {}",
            deepest_existing.display()
        );
    }
    let canonical_base = deepest_existing.canonicalize().with_context(|| {
        format!(
            "canonicalize destination ancestor {}",
            deepest_existing.display()
        )
    })?;
    if !suffix.as_os_str().is_empty() && !canonical_base.is_dir() {
        bail!(
            "resolved destination ancestor is not a directory: {}",
            canonical_base.display()
        );
    }
    Ok(canonical_base.join(suffix))
}

fn validate_resolved_git_destination(path: &Path) -> Result<PathBuf> {
    let absolute = lexical_absolute_path(path)?;
    let mut deepest_existing = absolute.clone();
    let metadata = loop {
        match fs::symlink_metadata(&deepest_existing) {
            Ok(metadata) => break metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !deepest_existing.pop() {
                    return Err(error)
                        .with_context(|| format!("resolve git destination {}", path.display()));
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect git destination ancestor {}",
                        deepest_existing.display()
                    )
                })
            }
        }
    };
    let suffix = absolute
        .strip_prefix(&deepest_existing)
        .with_context(|| format!("resolve git destination suffix for {}", absolute.display()))?;
    if !suffix.as_os_str().is_empty() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "git destination ancestor is not a directory: {}",
            deepest_existing.display()
        );
    }
    let canonical_base = deepest_existing.canonicalize().with_context(|| {
        format!(
            "canonicalize git destination ancestor {}",
            deepest_existing.display()
        )
    })?;
    if !suffix.as_os_str().is_empty() && !canonical_base.is_dir() {
        bail!(
            "resolved git destination ancestor is not a directory: {}",
            canonical_base.display()
        );
    }
    let resolved = canonical_base.join(suffix);
    if !is_safe_rule_root(parent_or_self(&resolved)) {
        bail!("resolved destination {} is not safe", resolved.display());
    }
    Ok(resolved)
}

fn lexical_absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve current directory for git destination")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
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
    let summary = describe_step(step, opts)?;
    Ok(ActionPlan {
        step_id: step.id.clone(),
        step_name: step_name(step),
        summary,
        prerequisites,
    })
}

/// Return a detailed, side-effect-free explanation of what one step will do.
///
/// This is shared by plans, reports, and the scenario inspector so the user
/// sees the same technical description before and during execution.
pub fn describe_step(step: &Step, opts: &RunOptions) -> Result<String> {
    Ok(match &step.action {
        Action::CreateDirectory(action) => format!(
            "create directory {} and missing parents; leave an existing real directory unchanged and never replace a file or symlink",
            action.path
        ),
        Action::InspectPath(action) => {
            let measurement = if action.recursive_size {
                ", recursively total regular-file bytes without following symlinks"
            } else {
                ""
            };
            let expectation = action
                .expect
                .as_ref()
                .map(describe_path_expectation)
                .unwrap_or_default();
            format!(
                "inspect {} for existence, type, emptiness, and timestamps{}{}",
                action.path, measurement, expectation
            )
        }
        Action::GitClone { repo, dest, branch } => format!(
            "ensure git repository {} at {}: clone when absent; otherwise fetch origin and fast-forward {} when safe (hooks and submodules disabled)",
            repo,
            dest,
            branch
                .as_deref()
                .map(|branch| format!("branch {branch}"))
                .unwrap_or_else(|| "the checked-out branch".into())
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
        Action::RunScript {
            interpreter,
            script,
            args,
            cwd,
            ..
        } => format!(
            "run {} script {}{}{}",
            script_interpreter_name(*interpreter),
            script,
            if args.is_empty() {
                String::new()
            } else {
                format!(" with arguments {:?}", args)
            },
            cwd.as_ref()
                .map(|directory| format!(" in {}", directory))
                .unwrap_or_default()
        ),
        Action::DownloadFile { url, dest, .. } => {
            format!("download {} to {} with sha256 verification", url, dest)
        }
        Action::ExtractArchive {
            src,
            dest,
            format,
            max_unpacked_bytes,
        } => {
            format!(
                "extract {} archive {} into {} atomically with traversal and link protection (maximum {} unpacked bytes)",
                archive_format_name(*format),
                src,
                dest,
                max_unpacked_bytes
            )
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
            "queue a native App Store {} download for application {} through Apple's system services",
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
    if let Action::RunScript { interpreter, .. } = &step.action {
        prerequisites.push(format!(
            "{} must be installed and available to ppduster",
            script_interpreter_requirement(*interpreter)
        ));
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

fn apply_step(step: &Step, opts: &RunOptions) -> Result<ApplyStepResult> {
    match &step.action {
        Action::CreateDirectory(action) => apply_create_directory(&action.path),
        Action::InspectPath(action) => {
            let metadata = inspect_path(action)?;
            if let Some(expectation) = &action.expect {
                verify_path_expectation(expectation, &metadata)?;
            }
            Ok(ApplyStepResult::AlreadySatisfied(summarize_path_metadata(
                &metadata,
            )))
        }
        Action::GitClone { repo, dest, branch } => {
            apply_git_clone_or_update(repo, dest, branch.as_deref())
        }
        Action::BrewInstall { package, cask } => {
            apply_brew_install(package, *cask).map(ApplyStepResult::Applied)
        }
        Action::RunCommand {
            program,
            args,
            cwd,
            env,
            shell,
        } => apply_run_command(program, args, cwd.as_deref(), env, *shell)
            .map(ApplyStepResult::Applied),
        Action::RunScript {
            interpreter,
            script,
            args,
            cwd,
            env,
        } => apply_run_script(*interpreter, script, args, cwd.as_deref(), env)
            .map(ApplyStepResult::Applied),
        Action::DownloadFile {
            url,
            dest,
            checksum,
        } => apply_download_file(url, dest, checksum).map(ApplyStepResult::Applied),
        Action::ExtractArchive {
            src,
            dest,
            format,
            max_unpacked_bytes,
        } => apply_extract_archive(src, dest, *format, *max_unpacked_bytes)
            .map(ApplyStepResult::Applied),
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
        )
        .map(ApplyStepResult::Applied),
        Action::InstallPkg { pkg, target } => {
            apply_install_pkg(pkg, target.as_deref()).map(ApplyStepResult::Applied)
        }
        Action::MacosRequirements {
            minimum_version,
            require_rosetta_on_apple_silicon,
        } => apply_macos_requirements(minimum_version, *require_rosetta_on_apple_silicon)
            .map(ApplyStepResult::Applied),
        Action::AppStoreInstall(action) => {
            apply_app_store_install(action.app_id, action.operation).map(ApplyStepResult::Applied)
        }
        Action::BambuStudioRelease(action) => {
            apply_bambu_studio_release(opts.release_channel.unwrap_or(action.channel))
                .map(ApplyStepResult::Applied)
        }
        Action::ActivateLicense(action) => {
            apply_activate_license(action.provider, action.method).map(ApplyStepResult::Applied)
        }
    }
}

fn apply_create_directory(raw_path: &str) -> Result<ApplyStepResult> {
    let path = validate_create_directory_path(raw_path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            return Ok(ApplyStepResult::AlreadySatisfied(format!(
                "directory already exists: {}",
                path.display()
            )))
        }
        Ok(_) => {
            // `validate_create_directory_path` reports the more specific
            // conflict; this protects against a race between both checks.
            validate_create_directory_path(raw_path)?;
            bail!(
                "directory destination changed while applying: {}",
                path.display()
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect directory {}", path.display()))
        }
    }

    fs::create_dir_all(&path)
        .with_context(|| format!("create directory and parents {}", path.display()))?;
    let verified = validate_create_directory_path(raw_path)?;
    let metadata = fs::symlink_metadata(&verified)
        .with_context(|| format!("verify created directory {}", verified.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "created path is not a real directory: {}",
            verified.display()
        );
    }
    Ok(ApplyStepResult::Applied(format!(
        "created directory: {}",
        verified.display()
    )))
}

fn inspect_path(action: &InspectPathAction) -> Result<PathMetadataOutput> {
    let path = validate_declared_path(&action.path)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PathMetadataOutput {
                path,
                exists: false,
                kind: None,
                size_bytes: None,
                empty: None,
                entry_count: None,
                modified_at: None,
                created_at: None,
            })
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect path metadata {}", path.display()))
        }
    };

    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        PathKind::Symlink
    } else if metadata.is_file() {
        PathKind::File
    } else if metadata.is_dir() {
        PathKind::Directory
    } else {
        PathKind::Other
    };
    let modified_at = metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .with_context(|| format!("read modified timestamp for {}", path.display()))?;
    let created_at = metadata.created().ok().map(DateTime::<Utc>::from);

    let (size_bytes, empty, entry_count) = match kind {
        PathKind::File => (Some(metadata.len()), Some(metadata.len() == 0), None),
        PathKind::Directory => {
            let empty = fs::read_dir(&path)
                .with_context(|| format!("read directory {}", path.display()))?
                .next()
                .transpose()
                .with_context(|| format!("inspect directory entry in {}", path.display()))?
                .is_none();
            let size_is_required = action.recursive_size
                || action.expect.as_ref().is_some_and(|expectation| {
                    expectation.min_size_bytes.is_some() || expectation.max_size_bytes.is_some()
                });
            if size_is_required {
                let (size, count) = measure_directory_tree(&path)?;
                (Some(size), Some(empty), Some(count))
            } else {
                (None, Some(empty), None)
            }
        }
        PathKind::Symlink => (None, None, None),
        PathKind::Other => (Some(metadata.len()), None, None),
    };

    Ok(PathMetadataOutput {
        path,
        exists: true,
        kind: Some(kind),
        size_bytes,
        empty,
        entry_count,
        modified_at: Some(modified_at),
        created_at,
    })
}

fn measure_directory_tree(path: &Path) -> Result<(u64, u64)> {
    let mut size_bytes = 0u64;
    let mut entry_count = 0u64;
    for entry in WalkDir::new(path).follow_links(false).into_iter().skip(1) {
        let entry = entry.with_context(|| format!("walk directory {}", path.display()))?;
        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| anyhow!("directory entry count overflow for {}", path.display()))?;
        if entry.file_type().is_file() {
            let length = entry
                .metadata()
                .with_context(|| format!("read file metadata {}", entry.path().display()))?
                .len();
            size_bytes = size_bytes.checked_add(length).ok_or_else(|| {
                anyhow!("directory size overflow while reading {}", path.display())
            })?;
        }
    }
    Ok((size_bytes, entry_count))
}

fn verify_path_expectation(
    expectation: &PathExpectation,
    metadata: &PathMetadataOutput,
) -> Result<()> {
    if let Some(expected) = expectation.exists {
        if metadata.exists != expected {
            bail!(
                "expected exists to be {}, observed {} for {}",
                expected,
                metadata.exists,
                metadata.path.display()
            );
        }
    }
    if !metadata.exists {
        if expectation.kind.is_some()
            || expectation.empty.is_some()
            || expectation.min_size_bytes.is_some()
            || expectation.max_size_bytes.is_some()
            || expectation.modified_at_or_after.is_some()
            || expectation.modified_at_or_before.is_some()
        {
            bail!(
                "path does not exist, so metadata assertions cannot be evaluated: {}",
                metadata.path.display()
            );
        }
        return Ok(());
    }

    if let Some(expected) = expectation.kind {
        let observed = metadata
            .kind
            .ok_or_else(|| anyhow!("path type is unavailable for {}", metadata.path.display()))?;
        if observed != expected {
            bail!(
                "expected kind {}, observed {} for {}",
                path_kind_name(expected),
                path_kind_name(observed),
                metadata.path.display()
            );
        }
    }
    if let Some(expected) = expectation.empty {
        let observed = metadata
            .empty
            .ok_or_else(|| anyhow!("emptiness is unavailable for {}", metadata.path.display()))?;
        if observed != expected {
            bail!(
                "expected empty to be {}, observed {} for {}",
                expected,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(minimum) = expectation.min_size_bytes {
        let observed = metadata
            .size_bytes
            .ok_or_else(|| anyhow!("size is unavailable for {}", metadata.path.display()))?;
        if observed < minimum {
            bail!(
                "expected size at least {} bytes, observed {} bytes for {}",
                minimum,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(maximum) = expectation.max_size_bytes {
        let observed = metadata
            .size_bytes
            .ok_or_else(|| anyhow!("size is unavailable for {}", metadata.path.display()))?;
        if observed > maximum {
            bail!(
                "expected size at most {} bytes, observed {} bytes for {}",
                maximum,
                observed,
                metadata.path.display()
            );
        }
    }
    if let Some(after) = expectation.modified_at_or_after.as_ref() {
        let observed = metadata.modified_at.as_ref().ok_or_else(|| {
            anyhow!(
                "modified timestamp is unavailable for {}",
                metadata.path.display()
            )
        })?;
        if observed < after {
            bail!(
                "expected modified_at at or after {}, observed {} for {}",
                format_timestamp(after),
                format_timestamp(observed),
                metadata.path.display()
            );
        }
    }
    if let Some(before) = expectation.modified_at_or_before.as_ref() {
        let observed = metadata.modified_at.as_ref().ok_or_else(|| {
            anyhow!(
                "modified timestamp is unavailable for {}",
                metadata.path.display()
            )
        })?;
        if observed > before {
            bail!(
                "expected modified_at at or before {}, observed {} for {}",
                format_timestamp(before),
                format_timestamp(observed),
                metadata.path.display()
            );
        }
    }
    Ok(())
}

fn summarize_path_metadata(metadata: &PathMetadataOutput) -> String {
    if !metadata.exists {
        return format!("path does not exist: {}", metadata.path.display());
    }
    let mut fields = vec![
        format!("path exists: {}", metadata.path.display()),
        format!(
            "kind: {}",
            metadata.kind.map(path_kind_name).unwrap_or("unknown")
        ),
    ];
    if let Some(size) = metadata.size_bytes {
        fields.push(format!("size_bytes: {size}"));
    }
    if let Some(empty) = metadata.empty {
        fields.push(format!("empty: {empty}"));
    }
    if let Some(count) = metadata.entry_count {
        fields.push(format!("entries: {count}"));
    }
    if let Some(modified) = metadata.modified_at.as_ref() {
        fields.push(format!("modified_at: {}", format_timestamp(modified)));
    }
    if let Some(created) = metadata.created_at.as_ref() {
        fields.push(format!("created_at: {}", format_timestamp(created)));
    }
    fields.join("; ")
}

fn describe_path_expectation(expectation: &PathExpectation) -> String {
    let mut requirements = Vec::new();
    if let Some(exists) = expectation.exists {
        requirements.push(format!("exists = {exists}"));
    }
    if let Some(kind) = expectation.kind {
        requirements.push(format!("kind = {}", path_kind_name(kind)));
    }
    if let Some(empty) = expectation.empty {
        requirements.push(format!("empty = {empty}"));
    }
    if let Some(minimum) = expectation.min_size_bytes {
        requirements.push(format!("size >= {minimum} bytes"));
    }
    if let Some(maximum) = expectation.max_size_bytes {
        requirements.push(format!("size <= {maximum} bytes"));
    }
    if let Some(after) = expectation.modified_at_or_after.as_ref() {
        requirements.push(format!("modified_at >= {}", format_timestamp(after)));
    }
    if let Some(before) = expectation.modified_at_or_before.as_ref() {
        requirements.push(format!("modified_at <= {}", format_timestamp(before)));
    }
    if requirements.is_empty() {
        String::new()
    } else {
        format!("; require all of: {}", requirements.join(", "))
    }
}

fn path_kind_name(kind: PathKind) -> &'static str {
    match kind {
        PathKind::File => "file",
        PathKind::Directory => "directory",
        PathKind::Symlink => "symlink",
        PathKind::Other => "other",
    }
}

fn format_timestamp(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn apply_git_clone_or_update(
    repo: &str,
    dest: &str,
    branch: Option<&str>,
) -> Result<ApplyStepResult> {
    let dest_path = expand_required_path(dest)?;
    validate_resolved_git_destination(&dest_path)?;
    if dest_path.exists() && fs::symlink_metadata(&dest_path)?.file_type().is_symlink() {
        bail!(
            "git destination must not be a symlink: {}",
            dest_path.display()
        );
    }

    let destination_is_empty = dest_path.is_dir()
        && fs::read_dir(&dest_path)
            .with_context(|| format!("inspect git destination {}", dest_path.display()))?
            .next()
            .is_none();
    if !dest_path.exists() || destination_is_empty {
        return clone_git_repository(repo, &dest_path, branch);
    }

    validate_existing_git_repository(&dest_path, repo)?;
    let active_branch = current_git_branch(&dest_path)?;
    let target_branch = match branch {
        Some(branch) => branch.to_owned(),
        None => active_branch.clone().ok_or_else(|| {
            anyhow!(
                "git destination {} has a detached HEAD; set an explicit branch for synchronization",
                dest_path.display()
            )
        })?,
    };
    validate_git_branch_name(&target_branch)?;

    let remote_ref = format!("refs/remotes/origin/{target_branch}");
    let local_ref = format!("refs/heads/{target_branch}");
    let refspec = format!("+refs/heads/{target_branch}:{remote_ref}");
    git_stdout(
        &dest_path,
        &[
            "fetch",
            "--no-tags",
            "--recurse-submodules=no",
            "origin",
            &refspec,
        ],
        &format!("fetch origin/{target_branch}"),
    )?;

    let remote_sha = git_ref_sha(&dest_path, &remote_ref)?.ok_or_else(|| {
        anyhow!(
            "origin does not provide branch {} for repository at {}",
            target_branch,
            dest_path.display()
        )
    })?;
    let Some(local_sha) = git_ref_sha(&dest_path, &local_ref)? else {
        if active_branch.as_deref() == Some(target_branch.as_str()) {
            bail!(
                "local branch {} has no commit at {}; refusing to overwrite its working tree",
                target_branch,
                dest_path.display()
            );
        }
        update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, &target_branch)?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "repository already existed at {}; local {} was missing and was created at {}{}",
            dest_path.display(),
            target_branch,
            short_git_sha(&remote_sha),
            preservation_note
        )));
    };

    if local_sha == remote_sha {
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "repository already existed at {}; {} branch ref was already up to date at {}{}",
            dest_path.display(),
            target_branch,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }

    if git_is_ancestor(&dest_path, &local_ref, &remote_ref)? {
        let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
        if active_branch.as_deref() == Some(target_branch.as_str()) {
            if let Some(blocker) = git_fast_forward_blocker(&dest_path, &local_ref, &remote_ref)? {
                bail!(
                    "repository already existed at {}; {} was outdated by {} commit(s), but {}; fetched origin/{} and left local files unchanged",
                    dest_path.display(),
                    target_branch,
                    behind,
                    blocker,
                    target_branch
                );
            }
            git_stdout(
                &dest_path,
                &["merge", "--ff-only", &remote_ref],
                &format!("fast-forward {target_branch}"),
            )?;
        } else {
            update_inactive_git_branch(&dest_path, &local_ref, &remote_ref, &target_branch)?;
        }
        let updated_sha = git_ref_sha(&dest_path, &local_ref)?.ok_or_else(|| {
            anyhow!(
                "local branch {} disappeared while updating {}",
                target_branch,
                dest_path.display()
            )
        })?;
        if updated_sha != remote_sha {
            bail!(
                "local branch {} changed concurrently while updating {}",
                target_branch,
                dest_path.display()
            );
        }
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::Applied(format!(
            "repository already existed at {}; {} was outdated by {} commit(s) and was updated {} -> {}{}",
            dest_path.display(),
            target_branch,
            behind,
            short_git_sha(&local_sha),
            short_git_sha(&updated_sha),
            preservation_note
        )));
    }

    if git_is_ancestor(&dest_path, &remote_ref, &local_ref)? {
        let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
        let preservation_note =
            checkout_preservation_note(&dest_path, active_branch.as_deref(), &target_branch)?;
        return Ok(ApplyStepResult::AlreadySatisfied(format!(
            "repository already existed at {}; {} already contains origin/{} and is ahead by {} local commit(s); left unchanged at {}{}",
            dest_path.display(),
            target_branch,
            target_branch,
            ahead,
            short_git_sha(&local_sha),
            preservation_note
        )));
    }

    let ahead = git_commit_count(&dest_path, &format!("{remote_ref}..{local_ref}"))?;
    let behind = git_commit_count(&dest_path, &format!("{local_ref}..{remote_ref}"))?;
    bail!(
        "repository already existed at {}; {} diverged from origin/{} (ahead {}, behind {}); fetched the remote branch but left local history unchanged",
        dest_path.display(),
        target_branch,
        target_branch,
        ahead,
        behind
    )
}

fn git_fast_forward_blocker(
    dest_path: &Path,
    local_ref: &str,
    remote_ref: &str,
) -> Result<Option<String>> {
    let tracked_status = git_stdout(
        dest_path,
        &["status", "--porcelain=v1", "--untracked-files=no"],
        "inspect tracked git working tree state",
    )?;
    if !tracked_status.is_empty() {
        return Ok(Some(
            "the working tree has tracked or staged local changes".into(),
        ));
    }

    let incoming_paths = git_nul_paths(
        dest_path,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--no-ext-diff",
            "-z",
            local_ref,
            remote_ref,
        ],
        "inspect incoming git paths",
    )?;
    let mut local_paths = git_nul_paths(
        dest_path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        "inspect untracked git paths",
    )?;
    local_paths.extend(git_nul_paths(
        dest_path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        "inspect ignored git paths",
    )?);
    if let Some(local_path) = local_paths.iter().find(|local_path| {
        incoming_paths.iter().any(|incoming_path| {
            local_path == &incoming_path
                || local_path.starts_with(incoming_path)
                || incoming_path.starts_with(local_path)
        })
    }) {
        return Ok(Some(format!(
            "untracked or ignored path {} conflicts with the incoming fast-forward",
            local_path.display()
        )));
    }
    Ok(None)
}

fn git_nul_paths(dest_path: &Path, args: &[&str], action: &str) -> Result<Vec<PathBuf>> {
    let output = git_output(dest_path, args)?;
    if !output.status.success() {
        ensure_git_output_succeeded(output, action)?;
        unreachable!();
    }
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{action} returned a non-UTF-8 path; refusing the update"))?;
    Ok(stdout
        .split_terminator('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn checkout_preservation_note(
    dest_path: &Path,
    active_branch: Option<&str>,
    target_branch: &str,
) -> Result<String> {
    match active_branch {
        Some(active) if active != target_branch => {
            Ok(format!("; active branch {active} was preserved"))
        }
        None => Ok("; detached checkout was preserved".into()),
        Some(_) => {
            let status = git_stdout(
                dest_path,
                &["status", "--porcelain=v1", "--untracked-files=normal"],
                "inspect git working tree",
            )?;
            Ok(if status.is_empty() {
                String::new()
            } else {
                "; working tree has local changes that were left untouched".into()
            })
        }
    }
}

fn update_inactive_git_branch(
    dest_path: &Path,
    local_ref: &str,
    remote_ref: &str,
    branch: &str,
) -> Result<()> {
    let refspec = format!("{remote_ref}:{local_ref}");
    git_stdout(
        dest_path,
        &[
            "fetch",
            "--no-tags",
            "--recurse-submodules=no",
            ".",
            &refspec,
        ],
        &format!("fast-forward inactive local branch {branch}"),
    )?;
    Ok(())
}

fn clone_git_repository(
    repo: &str,
    dest_path: &Path,
    branch: Option<&str>,
) -> Result<ApplyStepResult> {
    if let Some(branch) = branch {
        validate_git_branch_name(branch)?;
    }
    let resolved_before = validate_resolved_git_destination(dest_path)?;
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create clone parent {}", parent.display()))?;
    }
    let resolved_after = validate_resolved_git_destination(dest_path)?;
    if resolved_after != resolved_before {
        bail!(
            "git destination changed while preparing clone: {}",
            dest_path.display()
        );
    }
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("clone")
        .arg("--no-recurse-submodules");
    if let Some(branch) = branch {
        command.arg("--branch").arg(branch);
    }
    command.arg(repo).arg(dest_path);
    let output = command
        .output()
        .with_context(|| format!("clone repository into {}", dest_path.display()))?;
    ensure_git_output_succeeded(output, "git clone")?;

    let cloned_branch = match branch {
        Some(branch) => branch.to_owned(),
        None => current_git_branch(dest_path)?.ok_or_else(|| {
            anyhow!(
                "cloned repository at {} has a detached HEAD",
                dest_path.display()
            )
        })?,
    };
    let sha = git_ref_sha(dest_path, &format!("refs/heads/{cloned_branch}"))?.ok_or_else(|| {
        anyhow!(
            "cloned repository at {} has no local branch {}",
            dest_path.display(),
            cloned_branch
        )
    })?;
    Ok(ApplyStepResult::Applied(format!(
        "repository was absent; cloned into {} with {} at {}",
        dest_path.display(),
        cloned_branch,
        short_git_sha(&sha)
    )))
}

fn validate_existing_git_repository(dest_path: &Path, expected_repo: &str) -> Result<()> {
    if !dest_path.is_dir() {
        bail!(
            "git destination exists but is not a directory: {}",
            dest_path.display()
        );
    }
    let top_level = git_stdout(
        dest_path,
        &["rev-parse", "--show-toplevel"],
        "inspect git repository",
    )?;
    let canonical_top = Path::new(&top_level)
        .canonicalize()
        .with_context(|| format!("canonicalize git top-level {top_level}"))?;
    let canonical_dest = dest_path
        .canonicalize()
        .with_context(|| format!("canonicalize git destination {}", dest_path.display()))?;
    if canonical_top != canonical_dest {
        bail!(
            "destination {} is not a git repository root",
            dest_path.display()
        );
    }
    let bare = git_stdout(
        dest_path,
        &["rev-parse", "--is-bare-repository"],
        "inspect git repository type",
    )?;
    if bare != "false" {
        bail!(
            "destination {} is not a non-bare git working tree",
            dest_path.display()
        );
    }
    let origin = git_stdout(
        dest_path,
        &["remote", "get-url", "origin"],
        "read git origin URL",
    )?;
    if normalize_git_remote(&origin) != normalize_git_remote(expected_repo) {
        bail!(
            "repository at {} has an origin that does not match the requested repository; left it unchanged",
            dest_path.display()
        );
    }
    Ok(())
}

fn validate_git_branch_name(branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("git branch must not be empty");
    }
    let output = Command::new("git")
        .args(["check-ref-format", "--branch", branch])
        .output()
        .context("validate git branch")?;
    ensure_git_output_succeeded(output, "validate git branch")?;
    Ok(())
}

fn normalize_git_remote(remote: &str) -> String {
    github_git_remote_identity(remote)
        .unwrap_or_else(|| remote.trim().trim_end_matches('/').to_owned())
}

fn github_git_remote_identity(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let lowercase = remote.to_ascii_lowercase();
    let marker = "github.com";
    let marker_start = lowercase.find(marker)?;
    if marker_start > 0 && !matches!(lowercase.as_bytes()[marker_start - 1], b'/' | b'@' | b':') {
        return None;
    }
    let mut remainder = remote.get(marker_start + marker.len()..)?;
    if let Some(after_colon) = remainder.strip_prefix(':') {
        if let Some((possible_port, path)) = after_colon.split_once('/') {
            remainder = if possible_port.chars().all(|ch| ch.is_ascii_digit()) {
                path
            } else {
                after_colon
            };
        } else {
            remainder = after_colon;
        }
    } else {
        remainder = remainder.strip_prefix('/')?;
    }
    let path = remainder.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut components = path.split('/');
    let owner = components.next()?;
    let repository = components.next()?;
    if owner.is_empty() || repository.is_empty() || components.next().is_some() {
        return None;
    }
    Some(format!(
        "github.com/{}/{}",
        owner.to_ascii_lowercase(),
        repository.to_ascii_lowercase()
    ))
}

fn current_git_branch(dest_path: &Path) -> Result<Option<String>> {
    let output = git_output(dest_path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    ensure_git_output_succeeded(output, "read current git branch")?;
    unreachable!()
}

fn git_ref_sha(dest_path: &Path, reference: &str) -> Result<Option<String>> {
    let output = git_output(dest_path, &["rev-parse", "--verify", reference])?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    Ok(None)
}

fn git_is_ancestor(dest_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(
        dest_path,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            ensure_git_output_succeeded(output, "compare git history")?;
            unreachable!()
        }
    }
}

fn git_commit_count(dest_path: &Path, range: &str) -> Result<u64> {
    git_stdout(
        dest_path,
        &["rev-list", "--count", range],
        "count git commits",
    )?
    .parse()
    .context("parse git commit count")
}

fn git_stdout(dest_path: &Path, args: &[&str], action: &str) -> Result<String> {
    let output = git_output(dest_path, args)?;
    ensure_git_output_succeeded(output, action)
}

fn git_output(dest_path: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(dest_path)
        .args(args)
        .output()
        .with_context(|| format!("run git in {}", dest_path.display()))
}

fn ensure_git_output_succeeded(output: std::process::Output, action: &str) -> Result<String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        output.status.exit_ok(action)?;
    }
    bail!("{} failed: {}", action, detail)
}

fn short_git_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
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

const MAX_ARCHIVE_ENTRIES: usize = 100_000;

fn archive_format_name(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::Auto => "auto-detected",
        ArchiveFormat::Zip => "zip",
        ArchiveFormat::Tar => "tar",
        ArchiveFormat::TarGz => "tar.gz",
        ArchiveFormat::TarBz2 => "tar.bz2",
        ArchiveFormat::TarXz => "tar.xz",
    }
}

fn detect_archive_format(path: &Path, requested: ArchiveFormat) -> Result<ArchiveFormat> {
    if requested != ArchiveFormat::Auto {
        return Ok(requested);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("archive file name is not valid UTF-8"))?
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        Ok(ArchiveFormat::TarBz2)
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        Ok(ArchiveFormat::TarXz)
    } else if name.ends_with(".tar") {
        Ok(ArchiveFormat::Tar)
    } else {
        bail!(
            "cannot detect archive format from {}; set format explicitly",
            path.display()
        )
    }
}

fn apply_extract_archive(
    src: &str,
    dest: &str,
    requested_format: ArchiveFormat,
    max_unpacked_bytes: u64,
) -> Result<String> {
    let src_path = expand_required_path(src)?;
    let dest_path = expand_required_path(dest)?;
    let source_metadata = fs::symlink_metadata(&src_path)
        .with_context(|| format!("inspect archive source {}", src_path.display()))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        bail!(
            "archive source is not a regular file: {}",
            src_path.display()
        );
    }
    if path_entry_exists(&dest_path)? {
        bail!(
            "archive destination already exists; refusing to merge or overwrite: {}",
            dest_path.display()
        );
    }
    let destination_parent = dest_path
        .parent()
        .ok_or_else(|| anyhow!("archive destination has no parent"))?;
    fs::create_dir_all(destination_parent).with_context(|| {
        format!(
            "create archive destination parent {}",
            destination_parent.display()
        )
    })?;
    require_real_directory(destination_parent)?;
    let format = detect_archive_format(&src_path, requested_format)?;
    let staging = create_archive_staging_directory(destination_parent)?;

    let extract_result = match format {
        ArchiveFormat::Zip => extract_zip_archive(&src_path, &staging, max_unpacked_bytes),
        ArchiveFormat::Tar
        | ArchiveFormat::TarGz
        | ArchiveFormat::TarBz2
        | ArchiveFormat::TarXz => {
            extract_tar_archive(&src_path, &staging, format, max_unpacked_bytes)
        }
        ArchiveFormat::Auto => unreachable!(),
    };
    if let Err(extract_err) = extract_result {
        if let Err(cleanup_err) = remove_archive_staging(destination_parent, &staging) {
            return Err(extract_err.context(format!(
                "also failed to remove archive staging directory: {cleanup_err:#}"
            )));
        }
        return Err(extract_err);
    }
    if let Err(commit_err) = fs::rename(&staging, &dest_path) {
        if let Err(cleanup_err) = remove_archive_staging(destination_parent, &staging) {
            return Err(commit_err).context(format!(
                "commit archive extraction failed and staging cleanup also failed: {cleanup_err:#}"
            ));
        }
        return Err(commit_err)
            .with_context(|| format!("commit extracted archive to {}", dest_path.display()));
    }
    Ok(format!(
        "extracted {} archive {} into {}",
        archive_format_name(format),
        src_path.display(),
        dest_path.display()
    ))
}

fn extract_zip_archive(src: &Path, staging: &Path, max_unpacked_bytes: u64) -> Result<()> {
    let file = File::open(src).with_context(|| format!("open zip archive {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read zip archive {}", src.display()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("archive contains too many entries: {}", archive.len());
    }
    let mut total_bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read zip entry {index}"))?;
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("zip entry has an unsafe path: {}", entry.name()))?;
        if entry.is_symlink() || (!entry.is_dir() && !entry.is_file()) {
            bail!(
                "archive links and special files are not allowed: {}",
                entry.name()
            );
        }
        let output = checked_archive_output_path(staging, &rel)?;
        if entry.is_dir() {
            fs::create_dir_all(&output)
                .with_context(|| format!("create extracted directory {}", output.display()))?;
            continue;
        }
        total_bytes = add_unpacked_size(total_bytes, entry.size(), max_unpacked_bytes)?;
        create_archive_parent(staging, &output)?;
        let mut output_file = create_extracted_file(&output)?;
        let copied = io::copy(&mut entry, &mut output_file)
            .with_context(|| format!("extract zip entry {}", entry.name()))?;
        if copied != entry.size() {
            bail!(
                "zip entry size changed while extracting {}: expected {}, wrote {}",
                entry.name(),
                entry.size(),
                copied
            );
        }
        set_safe_file_permissions(&output, entry.unix_mode())?;
    }
    Ok(())
}

fn extract_tar_archive(
    src: &Path,
    staging: &Path,
    format: ArchiveFormat,
    max_unpacked_bytes: u64,
) -> Result<()> {
    let file = File::open(src).with_context(|| format!("open tar archive {}", src.display()))?;
    let reader: Box<dyn Read> = match format {
        ArchiveFormat::Tar => Box::new(BufReader::new(file)),
        ArchiveFormat::TarGz => Box::new(flate2::read::GzDecoder::new(BufReader::new(file))),
        ArchiveFormat::TarBz2 => Box::new(bzip2::read::BzDecoder::new(BufReader::new(file))),
        ArchiveFormat::TarXz => Box::new(xz2::read::XzDecoder::new(BufReader::new(file))),
        _ => bail!("invalid tar archive format"),
    };
    let mut archive = tar::Archive::new(reader);
    let mut entry_count = 0usize;
    let mut total_bytes = 0u64;
    for entry in archive.entries().context("read tar archive entries")? {
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            bail!("archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let mut entry = entry.context("read tar archive entry")?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            bail!("archive links and special files are not allowed");
        }
        let rel = entry.path().context("read tar entry path")?.into_owned();
        let output = checked_archive_output_path(staging, &rel)?;
        if entry_type.is_dir() {
            fs::create_dir_all(&output)
                .with_context(|| format!("create extracted directory {}", output.display()))?;
            continue;
        }
        let size = entry.header().size().context("read tar entry size")?;
        total_bytes = add_unpacked_size(total_bytes, size, max_unpacked_bytes)?;
        create_archive_parent(staging, &output)?;
        let mode = entry.header().mode().ok();
        let mut output_file = create_extracted_file(&output)?;
        let copied = io::copy(&mut entry, &mut output_file)
            .with_context(|| format!("extract tar entry {}", rel.display()))?;
        if copied != size {
            bail!(
                "tar entry size changed while extracting {}: expected {}, wrote {}",
                rel.display(),
                size,
                copied
            );
        }
        set_safe_file_permissions(&output, mode)?;
    }
    Ok(())
}

fn checked_archive_output_path(staging: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("archive entry escapes the destination: {}", rel.display());
    }
    Ok(staging.join(rel))
}

fn create_archive_parent(staging: &Path, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .ok_or_else(|| anyhow!("archive entry has no parent"))?;
    if !parent.starts_with(staging) {
        bail!("archive entry parent escapes the staging directory");
    }
    fs::create_dir_all(parent)
        .with_context(|| format!("create extracted file parent {}", parent.display()))
}

fn create_extracted_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create extracted file {}", path.display()))
}

fn add_unpacked_size(current: u64, entry: u64, maximum: u64) -> Result<u64> {
    let total = current
        .checked_add(entry)
        .ok_or_else(|| anyhow!("archive unpacked size overflow"))?;
    if total > maximum {
        bail!("archive exceeds max_unpacked_bytes ({maximum})");
    }
    Ok(total)
}

#[cfg(unix)]
fn set_safe_file_permissions(path: &Path, archived_mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = archived_mode.is_some_and(|mode| mode & 0o111 != 0);
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("set safe permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_safe_file_permissions(_path: &Path, _archived_mode: Option<u32>) -> Result<()> {
    Ok(())
}

fn create_archive_staging_directory(parent: &Path) -> Result<PathBuf> {
    for index in 0..1_000u32 {
        let candidate = parent.join(format!(".ppduster-archive-{}-{index}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("create archive staging directory {}", candidate.display())
                })
            }
        }
    }
    bail!("could not allocate an archive staging directory")
}

fn remove_archive_staging(parent: &Path, staging: &Path) -> Result<()> {
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid archive staging path {}", staging.display()))?;
    if staging.parent() != Some(parent) || !name.starts_with(".ppduster-archive-") {
        bail!(
            "refusing to remove unexpected archive staging path {}",
            staging.display()
        );
    }
    fs::remove_dir_all(staging)
        .with_context(|| format!("remove archive staging directory {}", staging.display()))
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

fn native_app_store_operation(operation: AppStoreOperation) -> app_store_installer::StoreOperation {
    match operation {
        AppStoreOperation::Install => app_store_installer::StoreOperation::Install,
        AppStoreOperation::Get => app_store_installer::StoreOperation::Get,
    }
}

fn app_store_country() -> String {
    std::env::var("PPDUSTER_APP_STORE_COUNTRY").unwrap_or_else(|_| "US".into())
}

fn app_store_identity_matches(
    installed_adam_id: Option<u64>,
    installed_bundle_id: &str,
    target_adam_id: u64,
    target_bundle_id: &str,
) -> bool {
    installed_adam_id == Some(target_adam_id)
        || (!installed_bundle_id.is_empty()
            && !target_bundle_id.is_empty()
            && installed_bundle_id == target_bundle_id)
}

fn app_store_app_is_installed(app_id: u64) -> Result<bool> {
    let installed = app_store::scan_installed(&[])
        .with_context(|| format!("scan installed Mac App Store applications for {app_id}"))?;
    let catalog = app_store::lookup_by_adam_id(app_id, &app_store_country())
        .with_context(|| format!("look up Mac App Store application {app_id}"))?;

    Ok(installed.apps.iter().any(|local| {
        catalog.apps.iter().any(|available| {
            available.adam_id == app_id
                && app_store_identity_matches(
                    local.adam_id,
                    &local.bundle_id,
                    app_id,
                    &available.bundle_id,
                )
        }) || local.adam_id == Some(app_id)
    }))
}

fn apply_app_store_install(app_id: u64, operation: AppStoreOperation) -> Result<String> {
    if !cfg!(target_os = "macos") {
        bail!("app-store-install is only supported on macOS");
    }
    if app_store_app_is_installed(app_id)? {
        return Ok(format!(
            "Mac App Store application {} is already installed",
            app_id
        ));
    }
    const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    let queued = app_store_installer::queue(
        app_id,
        native_app_store_operation(operation),
        INITIAL_REQUEST_TIMEOUT,
    )
    .with_context(|| {
        format!(
            "queue native App Store {} download for application {}",
            app_store_operation_name(operation),
            app_id
        )
    })?;
    match &queued.status {
        app_store_installer::QueueStatus::Queued => Ok(format!(
            "native App Store download queued for application {} ({} download{})",
            app_id,
            queued.downloads_queued,
            if queued.downloads_queued == 1 {
                ""
            } else {
                "s"
            }
        )),
        app_store_installer::QueueStatus::Pending { detail } => Ok(format!(
            "native App Store request for application {} is pending; do not retry before rescanning: {}",
            app_id, detail
        )),
    }
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

fn apply_run_script(
    interpreter: ScriptInterpreter,
    script: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let process_cwd = std::env::current_dir().context("resolve current directory")?;
    let working_directory = cwd
        .map(expand_required_path)
        .transpose()?
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                process_cwd.join(path)
            }
        })
        .map(|path| validate_script_working_directory(&path))
        .transpose()?;
    let expanded_script = expand_required_path(script)?;
    let requested_script_path = if expanded_script.is_absolute() {
        expanded_script
    } else {
        working_directory
            .as_ref()
            .unwrap_or(&process_cwd)
            .join(expanded_script)
    };
    let metadata = fs::symlink_metadata(&requested_script_path)
        .with_context(|| format!("inspect script {}", requested_script_path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "script must not be a symlink: {}",
            requested_script_path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "script is not a regular file: {}",
            requested_script_path.display()
        );
    }
    let script_path = requested_script_path
        .canonicalize()
        .with_context(|| format!("resolve script {}", requested_script_path.display()))?;

    let expanded_args = expand_args(args)?;
    let mut missing = Vec::new();
    for program in script_interpreter_candidates(interpreter) {
        let mut command = Command::new(program);
        configure_script_command(&mut command, interpreter, &script_path, &expanded_args);
        if let Some(directory) = &working_directory {
            command.current_dir(directory);
        }
        for (key, value) in env {
            command.env(key, expand_env_value(value)?);
        }
        match command.status() {
            Ok(status) => {
                status.exit_ok(&format!(
                    "{} script {}",
                    script_interpreter_name(interpreter),
                    script_path.display()
                ))?;
                return Ok(format!(
                    "ran {} script {}",
                    script_interpreter_name(interpreter),
                    script_path.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push((*program).to_owned());
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "start {} interpreter {} for script {}",
                        script_interpreter_name(interpreter),
                        program,
                        script_path.display()
                    )
                });
            }
        }
    }

    bail!(
        "{} interpreter not found (tried {}); install it or make it available on PATH",
        script_interpreter_name(interpreter),
        missing.join(", ")
    )
}

fn validate_script_working_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect script working directory {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "script working directory must not be a symlink: {}",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "script working directory is not a directory: {}",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("resolve script working directory {}", path.display()))
}

fn configure_script_command(
    command: &mut Command,
    interpreter: ScriptInterpreter,
    script: &Path,
    args: &[OsString],
) {
    if matches!(interpreter, ScriptInterpreter::PowerShell) {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive"]);
        if cfg!(target_os = "windows") {
            command.args(["-ExecutionPolicy", "Bypass"]);
        }
        command.arg("-File");
    }
    command.arg(script).args(args);
}

fn script_interpreter_candidates(interpreter: ScriptInterpreter) -> &'static [&'static str] {
    match interpreter {
        ScriptInterpreter::Sh if cfg!(target_os = "windows") => &["sh.exe", "sh"],
        ScriptInterpreter::Sh => &["/bin/sh", "sh"],
        ScriptInterpreter::Bash if cfg!(target_os = "windows") => &["bash.exe", "bash"],
        ScriptInterpreter::Bash => &["bash", "/bin/bash"],
        ScriptInterpreter::PowerShell if cfg!(target_os = "windows") => {
            &["pwsh.exe", "powershell.exe"]
        }
        ScriptInterpreter::PowerShell => &["pwsh", "powershell"],
    }
}

fn script_interpreter_name(interpreter: ScriptInterpreter) -> &'static str {
    match interpreter {
        ScriptInterpreter::Sh => "sh",
        ScriptInterpreter::Bash => "Bash",
        ScriptInterpreter::PowerShell => "PowerShell",
    }
}

fn script_interpreter_requirement(interpreter: ScriptInterpreter) -> &'static str {
    match interpreter {
        ScriptInterpreter::Sh => "a POSIX sh interpreter",
        ScriptInterpreter::Bash => "the Bash interpreter",
        ScriptInterpreter::PowerShell => "PowerShell (pwsh, or Windows PowerShell on Windows)",
    }
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
    if let Action::CreateDirectory(action) = &step.action {
        let path = validate_create_directory_path(&action.path)?;
        return match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(Some(
                format!("directory already exists: {}", path.display()),
            )),
            Ok(_) => bail!(
                "directory destination is not a real directory: {}",
                path.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("inspect directory {}", path.display()))
            }
        };
    }
    // Repository presence is not repository freshness. A git-clone action must
    // reach its apply phase so it can fetch the requested branch and decide
    // whether the existing checkout is current, behind, ahead, or diverged.
    if matches!(&step.action, Action::GitClone { .. }) {
        return Ok(None);
    }
    if run_command_checks {
        if let Action::AppStoreInstall(action) = &step.action {
            if app_store_app_is_installed(action.app_id)? {
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
        ActivateLicenseAction, AppBundleIdentity, AppStoreInstallAction, Checksum,
        CreateDirectoryAction, InspectPathAction, PathExpectation, PathKind, Task,
        TrustRequirement,
    };
    use std::path::PathBuf;

    fn base_task(step: Step) -> Task {
        Task {
            id: "setup-dev".into(),
            name: "Setup dev".into(),
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![step],
        }
    }

    struct GitTestRepository {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        seed: PathBuf,
    }

    fn test_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-c")
            .arg("core.hooksPath=/dev/null")
            .arg("-c")
            .arg("commit.gpgSign=false")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_git_test_repository() -> GitTestRepository {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        fs::create_dir(&seed).unwrap();
        let remote_arg = remote.to_string_lossy().into_owned();
        test_git(temp.path(), &["init", "--bare", &remote_arg]);
        test_git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        test_git(&seed, &["init"]);
        test_git(&seed, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        test_git(&seed, &["config", "user.name", "ppduster tests"]);
        test_git(
            &seed,
            &["config", "user.email", "ppduster-tests@example.invalid"],
        );
        fs::write(seed.join("state.txt"), "initial\n").unwrap();
        test_git(&seed, &["add", "state.txt"]);
        test_git(&seed, &["commit", "-m", "initial"]);
        test_git(&seed, &["remote", "add", "origin", &remote_arg]);
        test_git(&seed, &["push", "--set-upstream", "origin", "main"]);
        GitTestRepository {
            _temp: temp,
            remote,
            seed,
        }
    }

    fn push_git_test_commit(repository: &GitTestRepository, contents: &str) -> String {
        fs::write(repository.seed.join("state.txt"), contents).unwrap();
        test_git(&repository.seed, &["add", "state.txt"]);
        test_git(&repository.seed, &["commit", "-m", contents.trim()]);
        test_git(&repository.seed, &["push", "origin", "main"]);
        test_git(&repository.seed, &["rev-parse", "HEAD"])
    }

    fn git_sync_task(remote: &Path, destination: &Path) -> Task {
        base_task(Step {
            id: "sync-repository".into(),
            name: "Sync repository".into(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: Some(destination.join(".git")),
                command_succeeds: None,
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::GitClone {
                repo: remote.to_string_lossy().into_owned(),
                dest: destination.to_string_lossy().into_owned(),
                branch: Some("main".into()),
            },
        })
    }

    fn apply_test_task(task: &Task) -> RunReport {
        run_task(
            task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap()
    }

    fn plain_step(id: &str, action: Action) -> Step {
        Step {
            id: id.into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action,
        }
    }

    #[test]
    fn create_directory_is_dry_run_safe_recursive_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("state/nested");
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: destination.to_string_lossy().into_owned(),
            }),
        ));

        let planned = run_task(&task, &RunOptions::default()).unwrap();
        assert!(matches!(planned.outcomes[0], ActionOutcome::Planned { .. }));
        assert!(!destination.exists());

        let applied = apply_test_task(&task);
        assert!(destination.is_dir());
        assert!(matches!(applied.outcomes[0], ActionOutcome::Applied { .. }));

        let sentinel = destination.join("keep.txt");
        fs::write(&sentinel, "keep").unwrap();
        let repeated = apply_test_task(&task);
        assert!(matches!(
            repeated.outcomes[0],
            ActionOutcome::AlreadySatisfied { .. }
        ));
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[test]
    fn create_directory_refuses_to_replace_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("occupied");
        fs::write(&destination, "valuable").unwrap();
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: destination.to_string_lossy().into_owned(),
            }),
        ));

        let error = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(format!("{error:#}").contains("not a directory"));
        assert_eq!(fs::read_to_string(destination).unwrap(), "valuable");
    }

    #[cfg(unix)]
    #[test]
    fn create_directory_refuses_a_symlink_destination() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let task = base_task(plain_step(
            "create-state",
            Action::CreateDirectory(CreateDirectoryAction {
                path: link.to_string_lossy().into_owned(),
            }),
        ));

        let error = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(format!("{error:#}").contains("must not be a symlink"));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn inspect_path_runs_in_dry_run_and_reports_recursive_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(temp.path().join("one.bin"), b"12345").unwrap();
        fs::write(nested.join("two.bin"), b"abc").unwrap();
        let task = base_task(plain_step(
            "inspect",
            Action::InspectPath(InspectPathAction {
                path: temp.path().to_string_lossy().into_owned(),
                recursive_size: true,
                expect: Some(PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::Directory),
                    empty: Some(false),
                    min_size_bytes: Some(8),
                    max_size_bytes: Some(8),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        assert!(report.plans.is_empty());
        assert!(matches!(report.outcomes[0], ActionOutcome::Observed { .. }));
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        let StepOutput::PathMetadata(metadata) =
            report.steps[0].output.as_ref().expect("path output");
        assert!(metadata.exists);
        assert_eq!(metadata.kind, Some(PathKind::Directory));
        assert_eq!(metadata.size_bytes, Some(8));
        assert_eq!(metadata.empty, Some(false));
        assert_eq!(metadata.entry_count, Some(3));
        assert!(metadata.modified_at.is_some());

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["steps"][0]["output"]["type"], "path-metadata");
        assert_eq!(json["steps"][0]["output"]["value"]["size_bytes"], 8);
    }

    #[test]
    fn inspect_path_can_assert_absence_without_failing() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let task = base_task(plain_step(
            "inspect-missing",
            Action::InspectPath(InspectPathAction {
                path: missing.to_string_lossy().into_owned(),
                recursive_size: true,
                expect: Some(PathExpectation {
                    exists: Some(false),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        let StepOutput::PathMetadata(metadata) = report.steps[0].output.as_ref().unwrap();
        assert!(!metadata.exists);
        assert_eq!(metadata.kind, None);
        assert_eq!(metadata.size_bytes, None);
        assert_eq!(metadata.modified_at, None);
    }

    #[test]
    fn failed_path_expectation_halts_later_steps_in_dry_run() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let mut task = base_task(plain_step(
            "require-file",
            Action::InspectPath(InspectPathAction {
                path: missing.to_string_lossy().into_owned(),
                recursive_size: false,
                expect: Some(PathExpectation {
                    exists: Some(true),
                    kind: Some(PathKind::File),
                    ..PathExpectation::default()
                }),
            }),
        ));
        task.steps.push(plain_step(
            "later",
            Action::RunCommand {
                program: "/usr/bin/true".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                shell: ShellMode::Forbidden,
            },
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("expected exists"));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.steps[1].status, StepStatus::Skipped));
    }

    #[test]
    fn path_timestamp_expectations_are_inclusive() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("state.txt");
        fs::write(&file, "state").unwrap();
        let initial = inspect_path(&InspectPathAction {
            path: file.to_string_lossy().into_owned(),
            recursive_size: false,
            expect: None,
        })
        .unwrap();
        let modified = initial.modified_at.unwrap();
        let task = base_task(plain_step(
            "inspect-time",
            Action::InspectPath(InspectPathAction {
                path: file.to_string_lossy().into_owned(),
                recursive_size: false,
                expect: Some(PathExpectation {
                    modified_at_or_after: Some(modified),
                    modified_at_or_before: Some(modified),
                    ..PathExpectation::default()
                }),
            }),
        ));

        let report = run_task(&task, &RunOptions::default()).unwrap();
        assert!(report.errors.is_empty());
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
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
    fn script_requires_shell_flag() {
        let task = base_task(Step {
            id: "script".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "/tmp/ppduster-script.sh".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            },
        });
        let err = run_task(&task, &RunOptions::default()).unwrap_err();
        assert!(err.to_string().contains("--allow-shell"));
    }

    #[cfg(unix)]
    #[test]
    fn run_script_uses_direct_arguments_environment_and_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("probe.sh");
        fs::write(
            &script,
            "printf '%s|%s|%s\\n' \"$1\" \"$PPDUSTER_SCRIPT_TEST\" \"$PWD\" > result.txt\n",
        )
        .unwrap();
        let mut env = BTreeMap::new();
        env.insert("PPDUSTER_SCRIPT_TEST".into(), "environment value".into());

        apply_run_script(
            ScriptInterpreter::Sh,
            &script.to_string_lossy(),
            &["argument value".into()],
            Some(&temp.path().to_string_lossy()),
            &env,
        )
        .unwrap();

        let expected_cwd = temp.path().canonicalize().unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("result.txt")).unwrap(),
            format!(
                "argument value|environment value|{}\n",
                expected_cwd.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_script_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.sh");
        let link = temp.path().join("script.sh");
        fs::write(&target, "exit 0\n").unwrap();
        symlink(&target, &link).unwrap();

        let err = apply_run_script(
            ScriptInterpreter::Sh,
            &link.to_string_lossy(),
            &[],
            None,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not be a symlink"));
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
        let summary = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024 * 1024,
        )
        .unwrap();
        assert!(summary.contains("extracted"));
        assert!(dest.join("payload.txt").exists());
    }

    #[test]
    fn zip_archive_is_detected_and_extracted() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("payload.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "bin/tool",
                SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(b"tool").unwrap();
        writer.finish().unwrap();

        apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024,
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("bin/tool")).unwrap(), b"tool");
    }

    #[test]
    fn zip_traversal_and_existing_destinations_are_rejected_atomically() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("unsafe.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("../escape.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"escape").unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Zip,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe path"));
        assert!(!tmp.path().join("escape.txt").exists());
        assert!(!dest.exists());

        fs::create_dir(&dest).unwrap();
        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Zip,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to merge or overwrite"));
    }

    #[test]
    fn archive_unpacked_size_limit_is_enforced() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("large.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("payload.bin", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&[0u8; 32]).unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            16,
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_unpacked_bytes"));
        assert!(!dest.exists());
    }

    #[test]
    fn archive_symlinks_are_rejected() {
        use zip::write::SimpleFileOptions;

        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("link.zip");
        let dest = tmp.path().join("out");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .add_symlink("link", "../outside", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();

        let err = apply_extract_archive(
            &archive.to_string_lossy(),
            &dest.to_string_lossy(),
            ArchiveFormat::Auto,
            1024,
        )
        .unwrap_err();
        assert!(err.to_string().contains("links and special files"));
        assert!(!dest.exists());
    }

    fn test_tar_bytes() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"compressed";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "payload.txt", &payload[..])
            .unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn compressed_tar_formats_are_auto_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let tar_bytes = test_tar_bytes();

        let gzip_path = tmp.path().join("payload.tgz");
        let mut gzip = flate2::write::GzEncoder::new(
            File::create(&gzip_path).unwrap(),
            flate2::Compression::default(),
        );
        gzip.write_all(&tar_bytes).unwrap();
        gzip.finish().unwrap();

        let bzip_path = tmp.path().join("payload.tar.bz2");
        let mut bzip = bzip2::write::BzEncoder::new(
            File::create(&bzip_path).unwrap(),
            bzip2::Compression::default(),
        );
        bzip.write_all(&tar_bytes).unwrap();
        bzip.finish().unwrap();

        let xz_path = tmp.path().join("payload.txz");
        let mut xz = xz2::write::XzEncoder::new(File::create(&xz_path).unwrap(), 6);
        xz.write_all(&tar_bytes).unwrap();
        xz.finish().unwrap();

        for (index, archive) in [gzip_path, bzip_path, xz_path].iter().enumerate() {
            let dest = tmp.path().join(format!("out-{index}"));
            apply_extract_archive(
                &archive.to_string_lossy(),
                &dest.to_string_lossy(),
                ArchiveFormat::Auto,
                1024,
            )
            .unwrap();
            assert_eq!(fs::read(dest.join("payload.txt")).unwrap(), b"compressed");
        }
    }

    #[test]
    fn satisfied_non_git_step_reports_reason() {
        let task = base_task(Step {
            id: "download".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: Some(crate::automation::task::Check {
                path_exists: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                command_succeeds: None,
            }),
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::DownloadFile {
                url: "https://example.com/release.bin".into(),
                dest: "$HOME/Library/Caches/release.bin".into(),
                checksum: Checksum {
                    sha256: "abc".into(),
                },
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
    fn git_repository_is_cloned_checked_and_fast_forwarded_with_clear_reports() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);

        let cloned = apply_test_task(&task);
        assert!(matches!(cloned.steps[0].status, StepStatus::Applied));
        assert!(cloned.steps[0].summary.contains("repository was absent"));
        assert!(cloned.steps[0].summary.contains("main at"));
        assert!(matches!(
            &cloned.outcomes[0],
            ActionOutcome::Applied { summary } if summary == &cloned.steps[0].summary
        ));
        assert_eq!(
            cloned.steps[0].logs.last().unwrap().message,
            cloned.steps[0].summary
        );

        let current_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        let current = apply_test_task(&task);
        assert!(matches!(current.steps[0].status, StepStatus::Satisfied));
        assert!(current.steps[0]
            .summary
            .contains("repository already existed"));
        assert!(current.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(matches!(
            &current.outcomes[0],
            ActionOutcome::AlreadySatisfied { reason } if reason == &current.steps[0].summary
        ));
        assert_eq!(
            current.steps[0].logs.last().unwrap().message,
            current.steps[0].summary
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), current_sha);

        let remote_sha = push_git_test_commit(&repository, "remote update\n");
        let updated = apply_test_task(&task);
        assert!(matches!(updated.steps[0].status, StepStatus::Applied));
        assert!(updated.steps[0]
            .summary
            .contains("repository already existed"));
        assert!(updated.steps[0].summary.contains("main was outdated"));
        assert!(updated.steps[0].summary.contains("was updated"));
        assert!(matches!(
            &updated.outcomes[0],
            ActionOutcome::Applied { summary } if summary == &updated.steps[0].summary
        ));
        assert_eq!(
            updated.steps[0].logs.last().unwrap().message,
            updated.steps[0].summary
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), remote_sha);
        assert_eq!(
            fs::read_to_string(destination.join("state.txt")).unwrap(),
            "remote update\n"
        );
        assert!(test_git(
            &destination,
            &["status", "--porcelain=v1", "--untracked-files=normal"]
        )
        .is_empty());
    }

    #[test]
    fn git_repository_refuses_to_overwrite_a_non_repository_destination() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep.txt"), "keep me\n").unwrap();

        let report = apply_test_task(&git_sync_task(&repository.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("git repository"));
        assert!(report.steps[0]
            .logs
            .last()
            .unwrap()
            .message
            .contains("failed"));
        assert_eq!(
            fs::read_to_string(destination.join("keep.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn git_repository_clones_into_an_existing_empty_destination() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("empty-checkout");
        fs::create_dir(&destination).unwrap();

        let report = apply_test_task(&git_sync_task(&repository.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.steps[0].summary.contains("repository was absent"));
        assert!(destination.join(".git").is_dir());
        assert_eq!(
            test_git(&destination, &["rev-parse", "HEAD"]),
            test_git(&repository.seed, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn git_repository_rejects_a_mismatched_origin_without_moving_head() {
        let actual = init_git_test_repository();
        let requested = init_git_test_repository();
        let destination = actual._temp.path().join("checkout");
        apply_test_task(&git_sync_task(&actual.remote, &destination));
        let original_head = test_git(&destination, &["rev-parse", "HEAD"]);
        let original_origin = test_git(&destination, &["remote", "get-url", "origin"]);

        let report = apply_test_task(&git_sync_task(&requested.remote, &destination));
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("origin that does not match"));
        assert_eq!(
            test_git(&destination, &["rev-parse", "HEAD"]),
            original_head
        );
        assert_eq!(
            test_git(&destination, &["remote", "get-url", "origin"]),
            original_origin
        );
    }

    #[test]
    fn git_remote_identity_accepts_github_transport_changes_but_not_distinct_local_paths() {
        assert_eq!(
            normalize_git_remote("https://github.com/OpenAI/example.git"),
            normalize_git_remote("git@github.com:openai/example.git")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com:22/OpenAI/example.git"),
            normalize_git_remote("https://github.com/openai/example")
        );
        assert_ne!(
            normalize_git_remote("/tmp/example"),
            normalize_git_remote("/tmp/example.git")
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_repository_rejects_a_destination_whose_ancestor_resolves_into_documents() {
        use std::os::unix::fs::symlink;

        let repository = init_git_test_repository();
        let link = repository._temp.path().join("destination-link");
        let documents = dirs::home_dir().unwrap().join("Documents");
        symlink(&documents, &link).unwrap();
        let unique_child = format!(
            ".ppduster-git-safety-{}",
            repository
                ._temp
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
        let destination = link.join(&unique_child).join("repository");
        let error = validate_resolved_git_destination(&destination)
            .unwrap_err()
            .to_string();

        assert!(error.contains("not safe"), "unexpected error: {error}");
        assert!(!documents.join(unique_child).exists());
    }

    #[test]
    fn git_repository_refuses_diverged_main_without_losing_local_history() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);

        test_git(&destination, &["config", "user.name", "ppduster tests"]);
        test_git(
            &destination,
            &["config", "user.email", "ppduster-tests@example.invalid"],
        );
        fs::write(destination.join("local.txt"), "local commit\n").unwrap();
        test_git(&destination, &["add", "local.txt"]);
        test_git(&destination, &["commit", "-m", "local commit"]);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        push_git_test_commit(&repository, "different remote update\n");

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("diverged"));
        assert!(report.errors[0].contains("left local history unchanged"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
    }

    #[test]
    fn git_repository_fetches_but_does_not_overwrite_dirty_outdated_main() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);

        fs::write(destination.join("state.txt"), "local work\n").unwrap();
        let remote_sha = push_git_test_commit(&repository, "remote update\n");
        let report = apply_test_task(&task);

        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("was outdated"));
        assert!(report.errors[0].contains("local changes"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/remotes/origin/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("state.txt")).unwrap(),
            "local work\n"
        );
    }

    #[test]
    fn git_repository_reports_local_changes_when_main_ref_is_current() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        fs::write(destination.join("untracked.txt"), "local work\n").unwrap();

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Satisfied));
        assert!(report.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(report.steps[0]
            .summary
            .contains("working tree has local changes that were left untouched"));
        assert_eq!(
            fs::read_to_string(destination.join("untracked.txt")).unwrap(),
            "local work\n"
        );
    }

    #[test]
    fn git_repository_does_not_overwrite_an_ignored_path_during_fast_forward() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);
        let local_sha = test_git(&destination, &["rev-parse", "HEAD"]);

        fs::write(destination.join(".git/info/exclude"), "ignored.txt\n").unwrap();
        fs::write(destination.join("ignored.txt"), "LOCAL SECRET\n").unwrap();
        assert!(test_git(
            &destination,
            &["status", "--porcelain=v1", "--untracked-files=normal"]
        )
        .is_empty());

        fs::write(repository.seed.join("ignored.txt"), "REMOTE CONTENT\n").unwrap();
        test_git(&repository.seed, &["add", "ignored.txt"]);
        test_git(
            &repository.seed,
            &["commit", "-m", "track previously ignored path"],
        );
        test_git(&repository.seed, &["push", "origin", "main"]);
        let remote_sha = test_git(&repository.seed, &["rev-parse", "HEAD"]);

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Failed));
        assert!(matches!(report.outcomes[0], ActionOutcome::Blocked));
        assert!(report.errors[0].contains("ignored path ignored.txt conflicts"));
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), local_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/remotes/origin/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("ignored.txt")).unwrap(),
            "LOCAL SECRET\n"
        );
    }

    #[test]
    fn git_repository_updates_inactive_main_without_switching_the_active_branch() {
        let repository = init_git_test_repository();
        let destination = repository._temp.path().join("checkout");
        let task = git_sync_task(&repository.remote, &destination);
        apply_test_task(&task);

        test_git(&destination, &["checkout", "-b", "feature"]);
        fs::write(destination.join("feature-work.txt"), "unfinished\n").unwrap();
        let feature_sha = test_git(&destination, &["rev-parse", "HEAD"]);
        let remote_sha = push_git_test_commit(&repository, "remote update\n");

        let report = apply_test_task(&task);
        assert!(matches!(report.steps[0].status, StepStatus::Applied));
        assert!(report.steps[0].summary.contains("main was outdated"));
        assert!(report.steps[0]
            .summary
            .contains("active branch feature was preserved"));
        assert_eq!(
            test_git(&destination, &["symbolic-ref", "--short", "HEAD"]),
            "feature"
        );
        assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), feature_sha);
        assert_eq!(
            test_git(&destination, &["rev-parse", "refs/heads/main"]),
            remote_sha
        );
        assert_eq!(
            fs::read_to_string(destination.join("feature-work.txt")).unwrap(),
            "unfinished\n"
        );

        let current = apply_test_task(&task);
        assert!(matches!(current.steps[0].status, StepStatus::Satisfied));
        assert!(current.steps[0]
            .summary
            .contains("main branch ref was already up to date"));
        assert!(current.steps[0]
            .summary
            .contains("active branch feature was preserved"));
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
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
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
            description: "Test setup scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
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

    #[cfg(unix)]
    #[test]
    fn run_script_passes_arguments_environment_and_working_directory_without_shell_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.sh");
        fs::write(
            &script,
            r#"printf '%s\n%s\n%s\n' "$1" "$PPDUSTER_SCRIPT_VALUE" "$PWD" > result.txt
"#,
        )
        .unwrap();
        let argument = "value with spaces; $(exit 91)";
        let environment = "environment with spaces; $(exit 92)";
        let task = base_task(Step {
            id: "script".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "verify.sh".into(),
                args: vec![argument.into()],
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: BTreeMap::from([("PPDUSTER_SCRIPT_VALUE".into(), environment.into())]),
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
        let result = fs::read_to_string(dir.path().join("result.txt")).unwrap();
        let lines = result.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], argument);
        assert_eq!(lines[1], environment);
        assert_eq!(
            PathBuf::from(lines[2]).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn powershell_script_command_uses_file_mode_without_command_concatenation() {
        let mut command = Command::new("pwsh");
        configure_script_command(
            &mut command,
            ScriptInterpreter::PowerShell,
            Path::new("script with spaces.ps1"),
            &[OsString::from("argument; Write-Error should-not-run")],
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.starts_with(&[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
        ]));
        assert!(args.iter().any(|arg| arg == "-File"));
        assert!(args.iter().any(|arg| arg == "script with spaces.ps1"));
        assert_eq!(args.last().unwrap(), "argument; Write-Error should-not-run");
        assert!(!args.iter().any(|arg| arg == "-Command"));
        if cfg!(target_os = "windows") {
            assert!(args
                .windows(2)
                .any(|pair| pair == ["-ExecutionPolicy", "Bypass"]));
        }
    }

    #[test]
    fn run_script_executes_powershell_with_direct_arguments_when_available() {
        let probe_dir = tempfile::tempdir().unwrap();
        let powershell_available = script_interpreter_candidates(ScriptInterpreter::PowerShell)
            .iter()
            .any(|program| {
                Command::new(program)
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "exit 0",
                    ])
                    .current_dir(probe_dir.path())
                    .status()
                    .is_ok_and(|status| status.success())
            });
        if !powershell_available {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("verify.ps1");
        fs::write(
            &script,
            r#"param([string]$Value)
$Content = @($Value, $env:PPDUSTER_SCRIPT_VALUE, (Get-Location).Path) -join "`n"
$Encoding = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path (Get-Location).Path "result.txt"), $Content, $Encoding)
"#,
        )
        .unwrap();
        let argument = "value with spaces; Write-Error should-not-run";
        let environment = "environment with spaces; Write-Error should-not-run";

        apply_run_script(
            ScriptInterpreter::PowerShell,
            &script.to_string_lossy(),
            &[argument.into()],
            Some(&dir.path().to_string_lossy()),
            &BTreeMap::from([("PPDUSTER_SCRIPT_VALUE".into(), environment.into())]),
        )
        .unwrap();

        let result = fs::read_to_string(dir.path().join("result.txt")).unwrap();
        let lines = result.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], argument);
        assert_eq!(lines[1], environment);
        assert_eq!(
            PathBuf::from(lines[2]).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
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
            description: "Test LightBurn preflight scenario.".into(),
            platform: crate::rules::Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
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
    fn app_store_install_plan_is_typed_unprivileged_and_reports_prerequisites() {
        let task = base_task(Step {
            id: "install-xcode".into(),
            name: String::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::AppStoreInstall(AppStoreInstallAction {
                app_id: 497_799_835,
                operation: AppStoreOperation::Install,
            }),
        });

        let report = run_task(&task, &RunOptions::default()).unwrap();

        assert!(report.plans[0]
            .summary
            .contains("native App Store install download for application 497799835"));
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
    fn app_store_identity_matching_and_native_operations_are_exact() {
        assert!(app_store_identity_matches(
            Some(497_799_835),
            "com.apple.dt.Xcode",
            497_799_835,
            "com.example.unrelated",
        ));
        assert!(app_store_identity_matches(
            None,
            "com.apple.dt.Xcode",
            497_799_835,
            "com.apple.dt.Xcode",
        ));
        assert!(!app_store_identity_matches(
            Some(409_183_694),
            "com.example.unrelated",
            497_799_835,
            "com.apple.dt.Xcode",
        ));
        assert_eq!(
            native_app_store_operation(AppStoreOperation::Install),
            app_store_installer::StoreOperation::Install
        );
        assert_eq!(
            native_app_store_operation(AppStoreOperation::Get),
            app_store_installer::StoreOperation::Get
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
