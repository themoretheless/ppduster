use crate::automation::task::{Action, ElevationPolicy, ShellMode, Step, Task};
use crate::rules::expand_path_template;
use crate::safety::{is_safe_rule_root, stays_under_root};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
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
    pub summary: String,
    pub already_satisfied: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionOutcome {
    Planned { action: ActionPlan },
    AlreadySatisfied { reason: String },
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub plans: Vec<ActionPlan>,
    pub outcomes: Vec<ActionOutcome>,
}

pub fn run_task(task: &Task, opts: &RunOptions) -> Result<RunReport> {
    let mut plans = Vec::new();
    let mut outcomes = Vec::new();
    for step in &task.steps {
        enforce_step_policy(step, opts)?;
        let satisfaction = is_satisfied(step, opts.apply)?;
        if let Some(reason) = satisfaction {
            outcomes.push(ActionOutcome::AlreadySatisfied { reason });
            continue;
        }
        let plan = plan_step(step, false)?;
        plans.push(plan.clone());
        if opts.apply {
            return Err(AutomationError::Message(
                "apply mode is intentionally not implemented until safety spec is complete".into(),
            )
            .into());
        }
        outcomes.push(ActionOutcome::Planned { action: plan });
    }
    Ok(RunReport {
        task_id: task.id.clone(),
        plans,
        outcomes,
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
                anyhow::bail!("step {} has unexpanded destination {}", step.id, dest);
            };
            if !is_safe_rule_root(parent_or_self(&path)) {
                anyhow::bail!("step {} destination {} blocked by safety", step.id, path.display());
            }
        }
        _ => {}
    }
    Ok(())
}

fn parent_or_self(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

fn plan_step(step: &Step, already_satisfied: bool) -> Result<ActionPlan> {
    let summary = match &step.action {
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
            format!("brew install {}{}", if *cask { "--cask " } else { "" }, package)
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
    };
    Ok(ActionPlan {
        step_id: step.id.clone(),
        summary,
        already_satisfied,
    })
}

fn is_satisfied(step: &Step, run_command_checks: bool) -> Result<Option<String>> {
    let Some(check) = &step.check else {
        return Ok(None);
    };
    if let Some(path) = &check.path_exists {
        let expanded = expand_path_template(&path.to_string_lossy()).unwrap_or_else(|| path.clone());
        if expanded.exists() {
            return Ok(Some(format!("path exists: {}", expanded.display())));
        }
    }
    if let Some(cmd) = &check.command_succeeds {
        if cmd.is_empty() || !run_command_checks {
            return Ok(None);
        }
        let status = std::process::Command::new(&cmd[0])
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
        assert!(!report.plans[0].already_satisfied);
    }

    #[test]
    fn apply_mode_blocked() {
        let task = base_task(Step {
            id: "brew".into(),
            name: String::new(),
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            action: Action::BrewInstall {
                package: "git".into(),
                cask: false,
            },
        });
        let err = run_task(
            &task,
            &RunOptions {
                apply: true,
                ..RunOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("apply mode"));
    }

    #[test]
    fn shell_requires_flag() {
        let task = base_task(Step {
            id: "cmd".into(),
            name: String::new(),
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
    fn satisfied_step_reports_reason() {
        let task = base_task(Step {
            id: "clone".into(),
            name: String::new(),
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
}
