//! Automation task runner — executes [`Task`] steps with dry-run support.
//!
//! # Modes
//! - [`RunMode::DryRun`] — logs each step but never performs I/O or launches
//!   processes. Safe to run at any time for previewing what would happen.
//! - [`RunMode::Apply`] — actually executes each step in order, stopping on
//!   the first error unless [`RunOptions::continue_on_error`] is set.
//!
//! # Step implementation status
//! | Step kind       | Status     |
//! |-----------------|------------|
//! | `run_command`   | Implemented |
//! | `download_file` | Implemented |
//! | `extract_archive` | Implemented |
//! | `brew_install`  | Stub (explicit error) |
//! | `clone_repo`    | Stub (explicit error) |
//! | `install_dmg`   | Stub (explicit error) |
//! | `install_pkg`   | Stub (explicit error) |

use crate::automation::task::{
    DownloadFileParams, ExtractArchiveParams, RunCommandParams, Step, StepKind, Task,
};
use serde::{Deserialize, Serialize};
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

// ── Run context ───────────────────────────────────────────────────────────────

/// Controls whether steps are actually executed or just previewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Log what would happen; perform no I/O or process execution.
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
#[derive(Debug, Clone)]
pub struct RunContext {
    /// Whether to actually execute (Apply) or only preview (DryRun).
    pub mode: RunMode,
    /// Default working directory for steps that don't specify one.
    pub working_dir: PathBuf,
}

impl RunContext {
    pub fn new(mode: RunMode) -> Self {
        RunContext {
            mode,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = dir.into();
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

/// Outcome of a single step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum StepOutcome {
    /// Step was executed and succeeded.
    Ok { message: Option<String> },
    /// Step was skipped because the runner is in DryRun mode.
    Skipped { preview: String },
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
    pub fn is_skipped(&self) -> bool {
        matches!(self, StepOutcome::Skipped { .. })
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
    /// True when every step completed without an error.
    pub fn success(&self) -> bool {
        !self.steps.iter().any(|s| s.outcome.is_err())
    }

    pub fn ok_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_ok()).count()
    }

    pub fn skipped_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_skipped()).count()
    }

    pub fn error_count(&self) -> usize {
        self.steps.iter().filter(|s| s.outcome.is_err()).count()
    }
}

// ── Runner entry point ────────────────────────────────────────────────────────

/// Execute all steps in `task` according to `ctx` and `opts`.
///
/// Returns a [`RunReport`] regardless of whether steps succeed; individual
/// step errors are recorded in the report. Returns `Err` only for structural
/// problems (empty step list, unrecoverable setup failures).
pub fn run_task(task: &Task, ctx: &RunContext, opts: &RunOptions) -> Result<RunReport, AutomationError> {
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
                StepOutcome::Skipped { .. } => "skipped (dry-run)".to_string(),
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
        "[automation] task '{}' complete: ok={} skipped={} errors={}",
        task.name,
        report.ok_count(),
        report.skipped_count(),
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
        StepKind::BrewInstall(_) => stub_outcome(idx, "brew_install"),
        StepKind::CloneRepo(_) => stub_outcome(idx, "clone_repo"),
        StepKind::InstallDmg(_) => stub_outcome(idx, "install_dmg"),
        StepKind::InstallPkg(_) => stub_outcome(idx, "install_pkg"),
    }
}

/// Returns the standard "not yet implemented" error outcome for stub steps.
fn stub_outcome(_idx: usize, kind: &str) -> StepOutcome {
    StepOutcome::Err {
        message: format!("step kind '{kind}' is not yet implemented"),
    }
}

// ── Implemented step handlers ─────────────────────────────────────────────────

fn handle_run_command(idx: usize, p: &RunCommandParams, ctx: &RunContext) -> StepOutcome {
    let preview = format!(
        "run: {} {}{}",
        p.command,
        p.args.join(" "),
        p.working_dir
            .as_deref()
            .map(|d| format!(" (in {d})"))
            .unwrap_or_default()
    );

    if ctx.mode.is_dry_run() {
        return StepOutcome::Skipped { preview };
    }

    let work_dir: PathBuf = p
        .working_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| ctx.working_dir.clone());

    let mut cmd = std::process::Command::new(&p.command);
    cmd.args(&p.args).current_dir(&work_dir);
    for (k, v) in &p.env {
        cmd.env(k, v);
    }

    match cmd.status() {
        Ok(status) if status.success() => StepOutcome::Ok {
            message: Some(format!("exit code 0")),
        },
        Ok(status) => StepOutcome::Err {
            message: format!(
                "step {idx} (run_command): command '{}' exited with {}",
                p.command,
                status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
            ),
        },
        Err(e) => StepOutcome::Err {
            message: format!("step {idx} (run_command): failed to spawn '{}': {e}", p.command),
        },
    }
}

fn handle_download_file(idx: usize, p: &DownloadFileParams, ctx: &RunContext) -> StepOutcome {
    let dest = expand_tilde(&p.destination);
    let preview = format!("download {} -> {}", p.url, dest.display());

    if ctx.mode.is_dry_run() {
        return StepOutcome::Skipped { preview };
    }

    // Check existing file
    if dest.exists() && !p.overwrite {
        return StepOutcome::Ok {
            message: Some(format!("skipped (already exists): {}", dest.display())),
        };
    }

    // Create parent directories
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return StepOutcome::Err {
                message: format!("step {idx} (download_file): create parent dirs: {e}"),
            };
        }
    }

    // Use curl (universally available on macOS and most Linux)
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["--fail", "--silent", "--show-error", "--location", "-o"])
        .arg(&dest)
        .arg(&p.url);

    match cmd.status() {
        Ok(s) if s.success() => StepOutcome::Ok {
            message: Some(format!("downloaded to {}", dest.display())),
        },
        Ok(s) => StepOutcome::Err {
            message: format!(
                "step {idx} (download_file): curl exited with {}",
                s.code().unwrap_or(-1)
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
    let preview = format!("extract {} -> {}", source.display(), dest.display());

    if ctx.mode.is_dry_run() {
        return StepOutcome::Skipped { preview };
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

    let ext = source
        .to_string_lossy()
        .to_lowercase();
    let is_zip = ext.ends_with(".zip");
    let is_tar = ext.ends_with(".tar.gz")
        || ext.ends_with(".tgz")
        || ext.ends_with(".tar.bz2")
        || ext.ends_with(".tar.xz")
        || ext.ends_with(".tar");

    if is_zip {
        let mut cmd = std::process::Command::new("unzip");
        cmd.arg("-q").arg(&source).arg("-d").arg(&dest);
        match cmd.status() {
            Ok(s) if s.success() => StepOutcome::Ok {
                message: Some(format!("extracted to {}", dest.display())),
            },
            Ok(s) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): unzip exited with {}", s.code().unwrap_or(-1)),
            },
            Err(e) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): failed to spawn unzip: {e}"),
            },
        }
    } else if is_tar {
        let mut cmd = std::process::Command::new("tar");
        cmd.arg("-xf").arg(&source).arg("-C").arg(&dest);
        if p.strip_components > 0 {
            cmd.arg(format!("--strip-components={}", p.strip_components));
        }
        match cmd.status() {
            Ok(s) if s.success() => StepOutcome::Ok {
                message: Some(format!("extracted to {}", dest.display())),
            },
            Ok(s) => StepOutcome::Err {
                message: format!("step {idx} (extract_archive): tar exited with {}", s.code().unwrap_or(-1)),
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
        DownloadFileParams, ExtractArchiveParams, RunCommandParams, Step, StepKind, Task,
    };

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

    // ── Dry-run: no side effects ──────────────────────────────────────────────

    #[test]
    fn dry_run_run_command_is_skipped() {
        let task = make_task(vec![run_command_step("false", vec![])]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert!(report.success(), "dry-run should not count skipped as failure");
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.ok_count(), 0);
        assert_eq!(report.error_count(), 0);
    }

    #[test]
    fn dry_run_download_file_is_skipped() {
        let step = Step {
            label: None,
            kind: StepKind::DownloadFile(DownloadFileParams {
                url: "https://example.com/file".into(),
                destination: "/tmp/ppduster_test_dl".into(),
                overwrite: false,
            }),
        };
        let task = make_task(vec![step]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.skipped_count(), 1);
        // Ensure file was NOT created
        assert!(!std::path::Path::new("/tmp/ppduster_test_dl").exists());
    }

    #[test]
    fn dry_run_extract_archive_is_skipped() {
        let step = Step {
            label: None,
            kind: StepKind::ExtractArchive(ExtractArchiveParams {
                source: "/tmp/nonexistent.tar.gz".into(),
                destination: "/tmp/ppduster_test_extract".into(),
                strip_components: 0,
            }),
        };
        let task = make_task(vec![step]);
        let report = run_task(&task, &dry_ctx(), &Default::default()).unwrap();
        assert_eq!(report.skipped_count(), 1);
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
        assert_eq!(report.skipped_count(), 3);
    }

    #[test]
    fn report_stops_at_first_error_by_default() {
        // Step 0 succeeds; step 1 will be a stub (brew_install = error);
        // step 2 should not be reached.
        use crate::automation::task::BrewInstallParams;
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
        let report = run_task(&task, &apply_ctx(), &opts).unwrap();
        // Steps 0 (ok) and 1 (err); step 2 not reached
        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.error_count(), 1);
        assert!(!report.success());
    }

    #[test]
    fn continue_on_error_runs_all_steps() {
        use crate::automation::task::BrewInstallParams;
        let task = make_task(vec![
            Step {
                label: None,
                kind: StepKind::BrewInstall(BrewInstallParams {
                    packages: vec!["git".into()],
                    cask: false,
                }),
            },
            Step {
                label: None,
                kind: StepKind::BrewInstall(BrewInstallParams {
                    packages: vec!["curl".into()],
                    cask: false,
                }),
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
        use crate::automation::task::BrewInstallParams;
        let task = make_task(vec![Step {
            label: None,
            kind: StepKind::BrewInstall(BrewInstallParams {
                packages: vec!["git".into()],
                cask: false,
            }),
        }]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
        if let StepOutcome::Err { message } = &report.steps[0].outcome {
            assert!(message.contains("not yet implemented"), "got: {message}");
        }
    }

    #[test]
    fn clone_repo_is_stub() {
        use crate::automation::task::CloneRepoParams;
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
        use crate::automation::task::InstallDmgParams;
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
        use crate::automation::task::InstallPkgParams;
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

    // ── Dispatch: run_command apply ───────────────────────────────────────────

    #[test]
    fn run_command_apply_success() {
        let task = make_task(vec![run_command_step("echo", vec!["hello"])]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert_eq!(report.ok_count(), 1);
        assert!(report.success());
    }

    #[test]
    fn run_command_apply_failure() {
        // `false` always exits with code 1
        let task = make_task(vec![run_command_step("false", vec![])]);
        let report = run_task(&task, &apply_ctx(), &Default::default()).unwrap();
        assert!(report.steps[0].outcome.is_err());
        assert!(!report.success());
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
            // Either "not found" or "unrecognised format"
            assert!(
                message.contains("not found") || message.contains("unrecognised"),
                "got: {message}"
            );
        }
    }
}
