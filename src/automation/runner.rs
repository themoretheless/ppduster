//! Automation task runner — executes [`Task`] steps with dry-run support.
//!
//! # Modes
//! - [`RunMode::DryRun`] — computes a [`PlannedAction`] for every step and
//!   returns the full plan without performing any I/O or process execution.
//! - [`RunMode::Apply`] — executes each step in order. Before executing, each
//!   step handler checks [`is_satisfied`] and returns
//!   [`StepOutcome::AlreadySatisfied`] if the desired state already holds
//!   (idempotency). Stops on the first error unless
//!   [`RunOptions::continue_on_error`] is set.
//!
//! # Testability
//! All process execution is routed through the [`ProcessRunner`] trait so
//! unit tests can inject a [`FakeProcessRunner`] instead of spawning real
//! child processes.
//!
//! # Step implementation status
//! | Step kind         | Status                 |
//! |-------------------|------------------------|
//! | `run_command`     | Implemented            |
//! | `download_file`   | Implemented            |
//! | `extract_archive` | Implemented            |
//! | `brew_install`    | Stub (explicit error)  |
//! | `clone_repo`      | Stub (explicit error)  |
//! | `install_dmg`     | Stub (explicit error)  |
//! | `install_pkg`     | Stub (explicit error)  |

use crate::automation::task::{
    DownloadFileParams, ExtractArchiveParams, RunCommandParams, Step, StepKind, Task,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use thiserror::Error;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("step {index} ({kind}): {message}")]
    StepFailed {
        index: usize,
        kind: String,
        message: String,
    },
    #[error("step {index} ({kind}): not yet implemented")]
    NotImplemented { index: usize, kind: String },
    #[error("task has no steps")]
    NoSteps,
}

// ── Process runner abstraction ────────────────────────────────────────────────

/// Outcome of spawning a process through [`ProcessRunner`].
#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
}

/// Abstraction over process execution, enabling test injection.
pub trait ProcessRunner: Send + Sync {
    /// Spawn `program` with `args`, `cwd`, and extra `env`, wait for it to
    /// finish, and return its outcome. Returns `Err` only if the process
    /// could not be spawned at all (e.g., binary not found).
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
    ) -> Result<ProcessOutcome, std::io::Error>;
}

/// Production implementation — wraps [`std::process::Command`].
pub struct RealProcessRunner;

impl ProcessRunner for RealProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: &std::path::Path,
        env: &HashMap<String, String>,
    ) -> Result<ProcessOutcome, std::io::Error> {
        let status = std::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .envs(env)
            .status()?;
        Ok(ProcessOutcome {
            exit_code: status.code(),
            success: status.success(),
        })
    }
}

/// Test double — returns configurable outcomes without spawning any process.
///
/// By default all commands succeed. Entries in `failures` are matched by
/// program name; matching programs return a non-zero exit.
#[cfg_attr(not(test), allow(dead_code))]
pub struct FakeProcessRunner {
    /// Program names that should be reported as failures (exit code 1).
    pub failures: Vec<String>,
    /// Record of every (program, args) call made.
    pub calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FakeProcessRunner {
    pub fn new() -> Self {
        FakeProcessRunner {
            failures: vec![],
            calls: std::sync::Mutex::new(vec![]),
        }
    }

    pub fn with_failures(failures: Vec<&str>) -> Self {
        FakeProcessRunner {
            failures: failures.into_iter().map(String::from).collect(),
            calls: std::sync::Mutex::new(vec![]),
        }
    }

    pub fn recorded_calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ProcessRunner for FakeProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _cwd: &std::path::Path,
        _env: &HashMap<String, String>,
    ) -> Result<ProcessOutcome, std::io::Error> {
        self.calls
            .lock()
            .unwrap()
            .push((program.to_string(), args.iter().map(|s| s.to_string()).collect()));
        let success = !self.failures.iter().any(|f| f == program);
        Ok(ProcessOutcome {
            exit_code: if success { Some(0) } else { Some(1) },
            success,
        })
    }
}

// ── Run context ───────────────────────────────────────────────────────────────

/// Controls whether steps are actually executed or only planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Compute the plan for every step; perform no I/O or process execution.
    DryRun,
    /// Execute steps for real.
    Apply,
}

impl RunMode {
    pub fn is_dry_run(self) -> bool {
        self == RunMode::DryRun
    }
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunMode::DryRun => write!(f, "dry-run"),
            RunMode::Apply => write!(f, "apply"),
        }
    }
}

/// Shared context passed to every step handler.
pub struct RunContext {
    /// Whether to actually execute (Apply) or only plan (DryRun).
    pub mode: RunMode,
    /// Default working directory for steps that don't specify one.
    pub working_dir: PathBuf,
    /// Process runner — inject [`FakeProcessRunner`] in tests.
    pub proc: Box<dyn ProcessRunner>,
}

impl RunContext {
    pub fn new(mode: RunMode) -> Self {
        RunContext {
            mode,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            proc: Box::new(RealProcessRunner),
        }
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = dir.into();
        self
    }

    pub fn with_runner(mut self, runner: Box<dyn ProcessRunner>) -> Self {
        self.proc = runner;
        self
    }
}

/// Tuning options for a run.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// If true, continue executing subsequent steps after a step error.
    /// The overall run is still reported as failed.
    pub continue_on_error: bool,
}

// ── Results ───────────────────────────────────────────────────────────────────

/// Human-readable description of what a step *would* do, produced in dry-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedAction {
    /// Short verb phrase: "run echo hello", "download https://… -> /tmp/f"
    pub description: String,
    /// True if `is_satisfied()` returned true at planning time — the step
    /// would be a no-op even in apply mode.
    pub already_satisfied: bool,
}

/// Outcome of a single step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StepOutcome {
    /// Step was executed and succeeded.
    Ok { message: Option<String> },
    /// Dry-run mode: step was not executed; contains what *would* happen.
    Planned { action: PlannedAction },
    /// Step was skipped because `is_satisfied()` returned true (idempotent).
    AlreadySatisfied { reason: String },
    /// Step failed with an error message.
    Err { message: String },
}

impl StepOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, StepOutcome::Ok { .. })
    }
    pub fn is_err(&self) -> bool {
        matches!(self, StepOutcome::Err { .. })
    }
    pub fn is_planned(&self) -> bool {
        matches!(self, StepOutcome::Planned { .. })
    }
    pub fn is_already_satisfied(&self) -> bool {
        matches!(self, StepOutcome::AlreadySatisfied { .. })
    }
}

/// Result for one step in the execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// 0-based index of this step in the task's step list.
    pub step_index: usize,
    /// Human-readable label (from `step.label` or the kind name).
    pub label: String,
    /// Kind identifier string, e.g. `"run_command"`.
    pub kind_label: String,
    /// What happened.
    pub outcome: StepOutcome,
    /// Wall-clock time spent on this step (zero in dry-run).
    #[serde(skip)]
    pub elapsed: Duration,
}

/// Aggregated report for a complete task run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Task name.
    pub task_name: String,
    /// Mode the run was executed in.
    pub mode: RunMode,
    /// Results, one per step, in execution order.
    pub steps: Vec<StepResult>,
    /// Total wall-clock time for the whole run.
    #[serde(skip)]
    pub total_elapsed: Duration,
}

impl RunReport {
    /// True when every step completed without an error (planned/satisfied are not errors).
    pub fn success(&self) -> bool {
        !self.steps.iter().any(|s| s.outcome.is_err())
    }

    pub fn ok_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_ok()).count()
    }

    pub fn planned_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_planned()).count()
    }

    pub fn already_satisfied_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_already_satisfied()).count()
    }

    pub fn error_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_err()).count()
    }

    /// Convenience: list the planned actions when mode is DryRun.
    pub fn planned_actions(&self) -> Vec<&PlannedAction> {
        self.steps
            .iter()
            .filter_map(|s| {
                if let StepOutcome::Planned { action } = &s.outcome {
                    Some(action)
                } else {
                    None
                }
            })
            .collect()
    }
}

// ── Runner entry point ────────────────────────────────────────────────────────

/// Execute all steps in `task` according to `ctx` and `opts`.
///
/// In [`RunMode::DryRun`] every step produces a [`StepOutcome::Planned`]
/// describing what *would* happen — no I/O is performed.
///
/// Returns a [`RunReport`] regardless of step success; individual errors are
/// recorded in the report. Returns `Err` only for structural problems.
pub fn run_task(
    task: &Task,
    ctx: &RunContext,
    opts: &RunOptions,
) -> Result<RunReport, AutomationError> {
    if task.steps.is_empty() {
        return Err(AutomationError::NoSteps);
    }

    eprintln!("[automation] starting task '{}' (mode={})", task.name, ctx.mode);

    let run_start = Instant::now();
    let mut step_results = Vec::with_capacity(task.steps.len());

    for (idx, step) in task.steps.iter().enumerate() {
        let step_start = Instant::now();
        let label = step
            .label
            .clone()
            .unwrap_or_else(|| step.kind.kind_label().to_string());
        let kind_label = step.kind.kind_label().to_string();

        eprintln!("[automation] step {idx}: {kind_label} — {label}");

        let outcome = dispatch_step(idx, step, ctx);
        let elapsed = step_start.elapsed();
        let failed = outcome.is_err();

        eprintln!(
            "[automation] step {idx} done in {:.2}s — {}",
            elapsed.as_secs_f64(),
            match &outcome {
                StepOutcome::Ok { .. } => "ok".to_string(),
                StepOutcome::Planned { action } => format!("planned: {}", action.description),
                StepOutcome::AlreadySatisfied { reason } => format!("already satisfied: {reason}"),
                StepOutcome::Err { message } => format!("ERROR: {message}"),
            }
        );

        step_results.push(StepResult {
            step_index: idx,
            label,
            kind_label,
            outcome,
            elapsed,
        });

        if failed && !opts.continue_on_error {
            break;
        }
    }

    let total_elapsed = run_start.elapsed();
    let report = RunReport {
        task_name: task.name.clone(),
        mode: ctx.mode,
        steps: step_results,
        total_elapsed,
    };

    eprintln!(
        "[automation] task '{}' complete: ok={} planned={} satisfied={} errors={}",
        task.name,
        report.ok_count(),
        report.planned_count(),
        report.already_satisfied_count(),
        report.error_count()
    );

    Ok(report)
}

// ── Step dispatch ─────────────────────────────────────────────────────────────

fn dispatch_step(idx: usize, step: &Step, ctx: &RunContext) -> StepOutcome {
    match &step.kind {
        StepKind::RunCommand(p) => handle_run_command(idx, p, ctx),
        StepKind::DownloadFile(p) => handle_download_file(idx, p, ctx),
        StepKind::ExtractArchive(p) => handle_extract_archive(idx, p, ctx),
        StepKind::BrewInstall(_) => stub_outcome("brew_install"),
        StepKind::CloneRepo(_) => stub_outcome("clone_repo"),
        StepKind::InstallDmg(_) => stub_outcome("install_dmg"),
        StepKind::InstallPkg(_) => stub_outcome("install_pkg"),
    }
}

/// Returns the standard "not yet implemented" error outcome for stub steps.
fn stub_outcome(kind: &str) -> StepOutcome {
    StepOutcome::Err {
        message: format!("step kind '{kind}' is not yet implemented"),
    }
}

// ── Idempotency helpers ───────────────────────────────────────────────────────

/// Returns `Some(reason)` if `download_file` is already satisfied.
fn download_satisfied(p: &DownloadFileParams) -> Option<String> {
    let dest = expand_tilde(&p.destination);
    if !p.overwrite && dest.exists() {
        Some(format!("file already exists: {}", dest.display()))
    } else {
        None
    }
}

/// Returns `Some(reason)` if `extract_archive` is already satisfied.
/// Heuristic: destination directory exists and is non-empty.
fn extract_satisfied(p: &ExtractArchiveParams) -> Option<String> {
    let dest = expand_tilde(&p.destination);
    if dest.is_dir() {
        let non_empty = std::fs::read_dir(&dest)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Some(format!("destination already populated: {}", dest.display()));
        }
    }
    None
}

// ── Implemented step handlers ─────────────────────────────────────────────────

fn handle_run_command(idx: usize, p: &RunCommandParams, ctx: &RunContext) -> StepOutcome {
    let action_desc = format!(
        "run: {} {}{}",
        p.command,
        p.args.join(" "),
        p.working_dir
            .as_deref()
            .map(|d| format!(" (in {d})"))
            .unwrap_or_default()
    );

    if ctx.mode.is_dry_run() {
        // run_command has no structural is_satisfied check (commands are
        // inherently not idempotent by default), so always_satisfied = false.
        return StepOutcome::Planned {
            action: PlannedAction {
                description: action_desc,
                already_satisfied: false,
            },
        };
    }

    let work_dir: PathBuf = p
        .working_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| ctx.working_dir.clone());

    let args: Vec<&str> = p.args.iter().map(String::as_str).collect();

    match ctx.proc.run(&p.command, &args, &work_dir, &p.env) {
        Ok(outcome) if outcome.success => StepOutcome::Ok {
            message: Some("exit code 0".into()),
        },
        Ok(outcome) => StepOutcome::Err {
            message: format!(
                "step {idx} (run_command): command '{}' exited with {}",
                p.command,
                outcome
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            ),
        },
        Err(e) => StepOutcome::Err {
            message: format!("step {idx} (run_command): failed to spawn '{}': {e}", p.command),
        },
    }
}

fn handle_download_file(idx: usize, p: &DownloadFileParams, ctx: &RunContext) -> StepOutcome {
    let dest = expand_tilde(&p.destination);
    let action_desc = format!("download {} -> {}", p.url, dest.display());

    if ctx.mode.is_dry_run() {
        let already_satisfied = download_satisfied(p).is_some();
        return StepOutcome::Planned {
            action: PlannedAction {
                description: action_desc,
                already_satisfied,
            },
        };
    }

    // Idempotency check
    if let Some(reason) = download_satisfied(p) {
        return StepOutcome::AlreadySatisfied { reason };
    }

    // Create parent directories
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return StepOutcome::Err {
                message: format!("step {idx} (download_file): create parent dirs: {e}"),
            };
        }
    }

    let dest_str = dest.to_string_lossy().to_string();
    match ctx.proc.run(
        "curl",
        &["--fail", "--silent", "--show-error", "--location", "-o", &dest_str, &p.url],
        &ctx.working_dir,
        &HashMap::new(),
    ) {
        Ok(o) if o.success => StepOutcome::Ok {
            message: Some(format!("downloaded to {}", dest.display())),
        },
        Ok(o) => StepOutcome::Err {
            message: format!(
                "step {idx} (download_file): curl exited with {}",
                o.exit_code.unwrap_or(-1)
            ),
        },
        Err(e) => StepOutcome::Err {
            message: format!("step {idx} (download_file): failed to spawn curl: {e}"),
        },
    }
}

fn handle_extract_archive(idx: usize, p: &ExtractArchiveParams, ctx: &RunContext) -> StepOutcome {
    let source = expand_tilde(&p.source);
    let dest = expand_tilde(&p.destination);
    let action_desc = format!("extract {} -> {}", source.display(), dest.display());

    if ctx.mode.is_dry_run() {
        let already_satisfied = extract_satisfied(p).is_some();
        return StepOutcome::Planned {
            action: PlannedAction {
                description: action_desc,
                already_satisfied,
            },
        };
    }

    // Idempotency check
    if let Some(reason) = extract_satisfied(p) {
        return StepOutcome::AlreadySatisfied { reason };
    }

    if !source.exists() {
        return StepOutcome::Err {
            message: format!("step {idx} (extract_archive): source not found: {}", source.display()),
        };
    }

    if let Err(e) = std::fs::create_dir_all(&dest) {
        return StepOutcome::Err {
            message: format!("step {idx} (extract_archive): create dest dir: {e}"),
        };
    }

    let ext = source.to_string_lossy().to_lowercase();
    let is_zip = ext.ends_with(".zip");
    let is_tar = ext.ends_with(".tar.gz")
        || ext.ends_with(".tgz")
        || ext.ends_with(".tar.bz2")
        || ext.ends_with(".tar.xz")
        || ext.ends_with(".tar");

    let dest_str = dest.to_string_lossy().to_string();
    let source_str = source.to_string_lossy().to_string();

    if is_zip {
        let args = ["-q", &source_str, "-d", &dest_str];
        match ctx.proc.run("unzip", &args, &ctx.working_dir, &HashMap::new()) {
            Ok(o) if o.success => StepOutcome::Ok {
                message: Some(format!("extracted to {}", dest.display())),
            },
            Ok(o) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): unzip exited with {}", o.exit_code.unwrap_or(-1)),
            },
            Err(e) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): failed to spawn unzip: {e}"),
            },
        }
    } else if is_tar {
        let strip = format!("--strip-components={}", p.strip_components);
        let mut args: Vec<&str> = vec!["-xf", &source_str, "-C", &dest_str];
        if p.strip_components > 0 {
            args.push(&strip);
        }
        match ctx.proc.run("tar", &args, &ctx.working_dir, &HashMap::new()) {
            Ok(o) if o.success => StepOutcome::Ok {
                message: Some(format!("extracted to {}", dest.display())),
            },
            Ok(o) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): tar exited with {}", o.exit_code.unwrap_or(-1)),
            },
            Err(e) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): failed to spawn tar: {e}"),
            },
        }
    } else {
        StepOutcome::Err {
            message: format!(
                "step {idx} (extract_archive): unrecognised archive format for '{}'",
                source.display()
            ),
        }
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~/") || path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.join(path.trim_start_matches("~/"));
        }
    }
    PathBuf::from(path)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::task::{
        BrewInstallParams, CloneRepoParams, DownloadFileParams, ExtractArchiveParams,
        InstallDmgParams, InstallPkgParams, RunCommandParams, Step, StepKind, Task,
    };
    use tempfile::TempDir;

    fn make_task(steps: Vec<Step>) -> Task {
        Task {
            name: "test-task".into(),
            description: String::new(),
            steps,
        }
    }

    fn run_command_step(cmd: &str, args: Vec<&str>) -> Step {
        Step {
            label: None,
            kind: StepKind::RunCommand(RunCommandParams {
                command: cmd.into(),
                args: args.into_iter().map(String::from).collect(),
                working_dir: None,
                env: Default::default(),
            }),
        }
    }

    fn dry_ctx() -> RunContext {
        RunContext::new(RunMode::DryRun)
    }

    fn apply_ctx() -> RunContext {
        RunContext::new(RunMode::Apply)
    }

    fn fake_apply(failures: Vec<&str>) -> RunContext {
        RunContext::new(RunMode::Apply)
            .with_runner(Box::new(FakeProcessRunner::with_failures(failures)))
    }

    // ── Dry-run: produces Planned outcomes, no I/O ────────────────────────────

    #[test]
    fn dry_run_run_command_is_planned() {
        let task = make_task(vec![run_command_step("false", vec![])]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert!(report.success(), "dry-run planned steps are not failures");
        assert_eq!(report.planned_count(), 1);
        assert_eq!(report.ok_count(), 0);
        assert_eq!(report.error_count(), 0);
        assert!(matches!(
            &report.steps[0].outcome,
            StepOutcome::Planned { action } if action.description.contains("false")
        ));
    }

    #[test]
    fn dry_run_produces_planned_action_description() {
        let task = make_task(vec![run_command_step("echo", vec!["hello", "world"])]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        let actions = report.planned_actions();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].description.contains("echo"));
        assert!(actions[0].description.contains("hello"));
    }

    #[test]
    fn dry_run_download_file_is_planned() {
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::DownloadFile(DownloadFileParams {
                url: "https://example.com/file".into(),
                destination: "/tmp/ppduster_test_dl_planned".into(),
                overwrite: false,
            }),
        }]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.planned_count(), 1);
        // Ensure file was NOT created
        assert!(!std::path::Path::new("/tmp/ppduster_test_dl_planned").exists());
    }

    #[test]
    fn dry_run_extract_archive_is_planned() {
        let step = Step {
            label: None,
            kind: StepKind::ExtractArchive(ExtractArchiveParams {
                source: "/tmp/nonexistent.tar.gz".into(),
                destination: "/tmp/ppduster_test_extract_planned".into(),
                strip_components: 0,
            }),
        };
        let task = make_task(vec![step]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.planned_count(), 1);
    }

    // ── Dry-run: already_satisfied flag in PlannedAction ─────────────────────

    #[test]
    fn dry_run_download_flags_already_satisfied_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("existing.tar.gz");
        std::fs::write(&dest, b"data").unwrap();

        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::DownloadFile(DownloadFileParams {
                url: "https://example.com/file".into(),
                destination: dest.to_string_lossy().into_owned(),
                overwrite: false,
            }),
        }]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        if let StepOutcome::Planned { action } = &report.steps[0].outcome {
            assert!(action.already_satisfied, "should flag file already exists");
        } else {
            panic!("expected Planned outcome");
        }
    }

    #[test]
    fn dry_run_extract_flags_already_satisfied_when_dest_populated() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("file.txt"), b"content").unwrap();

        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::ExtractArchive(ExtractArchiveParams {
                source: "/tmp/archive.tar.gz".into(),
                destination: dest.to_string_lossy().into_owned(),
                strip_components: 0,
            }),
        }]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        if let StepOutcome::Planned { action } = &report.steps[0].outcome {
            assert!(action.already_satisfied);
        } else {
            panic!("expected Planned outcome");
        }
    }

    // ── Idempotency (apply mode) ──────────────────────────────────────────────

    #[test]
    fn download_file_apply_already_satisfied() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("existing.tar.gz");
        std::fs::write(&dest, b"data").unwrap();

        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::DownloadFile(DownloadFileParams {
                url: "https://example.com/should-not-fetch".into(),
                destination: dest.to_string_lossy().into_owned(),
                overwrite: false,
            }),
        }]);
        // Use fake runner that would fail any curl call — if curl is invoked the test fails
        let ctx = fake_apply(vec!["curl"]);
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert!(report.success());
        assert_eq!(report.already_satisfied_count(), 1);
    }

    #[test]
    fn extract_archive_apply_already_satisfied() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("marker"), b"x").unwrap();

        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::ExtractArchive(ExtractArchiveParams {
                source: "/tmp/archive.tar.gz".into(),
                destination: dest.to_string_lossy().into_owned(),
                strip_components: 0,
            }),
        }]);
        let ctx = fake_apply(vec!["tar"]); // tar would fail if called
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert!(report.success());
        assert_eq!(report.already_satisfied_count(), 1);
    }

    // ── ProcessRunner injection (FakeProcessRunner) ───────────────────────────

    #[test]
    fn fake_runner_success_for_run_command() {
        let fake = FakeProcessRunner::new();
        let ctx = RunContext::new(RunMode::Apply).with_runner(Box::new(fake));
        let task = make_task(vec![run_command_step("my-tool", vec!["--flag"])]);
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert!(report.success());
        assert_eq!(report.ok_count(), 1);
    }

    #[test]
    fn fake_runner_records_calls() {
        use std::sync::Arc;
        // Use Arc<FakeProcessRunner> to read calls after run
        let fake = Arc::new(FakeProcessRunner::new());
        struct ArcRunner(Arc<FakeProcessRunner>);
        impl ProcessRunner for ArcRunner {
            fn run(&self, p: &str, a: &[&str], c: &std::path::Path, e: &HashMap<String, String>) -> Result<ProcessOutcome, std::io::Error> {
                self.0.run(p, a, c, e)
            }
        }
        let fake_clone = Arc::clone(&fake);
        let ctx = RunContext::new(RunMode::Apply).with_runner(Box::new(ArcRunner(fake_clone)));
        let task = make_task(vec![
            run_command_step("echo", vec!["a"]),
            run_command_step("echo", vec!["b"]),
        ]);
        run_task(&task, &ctx, &Default::default()).unwrap();
        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "echo");
        assert_eq!(calls[1].0, "echo");
    }

    #[test]
    fn fake_runner_failure_for_run_command() {
        let ctx = fake_apply(vec!["my-failing-tool"]);
        let task = make_task(vec![run_command_step("my-failing-tool", vec![])]);
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert!(!report.success());
        assert_eq!(report.error_count(), 1);
    }

    // ── Report accumulation ───────────────────────────────────────────────────

    #[test]
    fn report_accumulates_all_steps_dry_run() {
        let task = make_task(vec![
            run_command_step("echo", vec!["a"]),
            run_command_step("echo", vec!["b"]),
            run_command_step("echo", vec!["c"]),
        ]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.steps.len(), 3);
        assert_eq!(report.planned_count(), 3);
    }

    #[test]
    fn report_stops_at_first_error_by_default() {
        let task = make_task(vec![
            run_command_step("echo", vec!["ok"]),
            Step {
                label: None,
                kind: StepKind::BrewInstall(BrewInstallParams {
                    packages: vec!["git".into()],
                    cask: false,
                }),
            },
            run_command_step("echo", vec!["should not run"]),
        ]);
        let opts = RunOptions { continue_on_error: false };
        let report = run_task(&task, &fake_apply(vec![]), &opts).unwrap();
        // Steps 0 (ok) and 1 (err); step 2 not reached
        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.error_count(), 1);
        assert!(!report.success());
    }

    #[test]
    fn continue_on_error_runs_all_steps() {
        let task = make_task(vec![
            Step {
                label: None,
                kind: StepKind::BrewInstall(BrewInstallParams { packages: vec!["git".into()], cask: false }),
            },
            Step {
                label: None,
                kind: StepKind::BrewInstall(BrewInstallParams { packages: vec!["curl".into()], cask: false }),
            },
        ]);
        let opts = RunOptions { continue_on_error: true };
        let report = run_task(&task, &apply_ctx(), &opts).unwrap();
        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.error_count(), 2);
    }

    // ── Stub steps return explicit NotImplemented errors ──────────────────────

    #[test]
    fn brew_install_is_stub() {
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::BrewInstall(BrewInstallParams { packages: vec!["git".into()], cask: false }),
        }]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
        if let StepOutcome::Err { message } = &report.steps[0].outcome {
            assert!(message.contains("not yet implemented"), "got: {message}");
        }
    }

    #[test]
    fn clone_repo_is_stub() {
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::CloneRepo(CloneRepoParams {
                url: "https://github.com/example/repo.git".into(),
                destination: "/tmp/repo".into(),
                branch: None,
                shallow: false,
            }),
        }]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(matches!(&report.steps[0].outcome, StepOutcome::Err { message } if message.contains("not yet implemented")));
    }

    #[test]
    fn install_dmg_is_stub() {
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::InstallDmg(InstallDmgParams {
                source: "/tmp/App.dmg".into(),
                app_name: "App.app".into(),
                install_dir: "/Applications".into(),
            }),
        }]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(matches!(&report.steps[0].outcome, StepOutcome::Err { message } if message.contains("not yet implemented")));
    }

    #[test]
    fn install_pkg_is_stub() {
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::InstallPkg(InstallPkgParams {
                source: "/tmp/pkg.pkg".into(),
                target: "/".into(),
                sudo: false,
            }),
        }]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(matches!(&report.steps[0].outcome, StepOutcome::Err { message } if message.contains("not yet implemented")));
    }

    // ── Dispatch: run_command apply via fake runner ───────────────────────────

    #[test]
    fn run_command_apply_success_via_fake() {
        let ctx = fake_apply(vec![]);
        let task = make_task(vec![run_command_step("echo", vec!["hello"])]);
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert_eq!(report.ok_count(), 1);
        assert!(report.success());
    }

    #[test]
    fn run_command_apply_failure_via_fake() {
        let ctx = fake_apply(vec!["false"]);
        let task = make_task(vec![run_command_step("false", vec![])]);
        let report = run_task(&task, &ctx, &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
        assert!(!report.success());
    }

    // ── Apply: run_command via real process (integration) ────────────────────

    #[test]
    fn run_command_real_echo_succeeds() {
        let task = make_task(vec![run_command_step("echo", vec!["hello"])]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert_eq!(report.ok_count(), 1);
    }

    #[test]
    fn run_command_real_false_fails() {
        let task = make_task(vec![run_command_step("false", vec![])]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
    }

    // ── Empty task ────────────────────────────────────────────────────────────

    #[test]
    fn empty_task_returns_error() {
        let task = make_task(vec![]);
        let result = run_task(&task, &dry_ctx(), &Default::default());
        assert!(matches!(result, Err(AutomationError::NoSteps)));
    }

    // ── Step labels ───────────────────────────────────────────────────────────

    #[test]
    fn step_label_used_when_provided() {
        let step = Step {
            label: Some("my custom label".into()),
            kind: StepKind::RunCommand(RunCommandParams {
                command: "echo".into(),
                args: vec![],
                working_dir: None,
                env: Default::default(),
            }),
        };
        let task = make_task(vec![step]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.steps[0].label, "my custom label");
    }

    #[test]
    fn step_label_defaults_to_kind_label() {
        let task = make_task(vec![run_command_step("echo", vec![])]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.steps[0].label, "run_command");
    }

    // ── extract_archive: bad format ───────────────────────────────────────────

    #[test]
    fn extract_archive_unknown_format_is_error_in_apply() {
        let step = Step {
            label: None,
            kind: StepKind::ExtractArchive(ExtractArchiveParams {
                source: "/tmp/nonexistent.rar".into(),
                destination: "/tmp/ppduster_test_rar".into(),
                strip_components: 0,
            }),
        };
        let task = make_task(vec![step]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
        if let StepOutcome::Err { message } = &report.steps[0].outcome {
            assert!(
                message.contains("not found") || message.contains("unrecognised"),
                "got: {message}"
            );
        }
    }
}
