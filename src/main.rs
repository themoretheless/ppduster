use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use ppduster::automation;
use ppduster::clean;
use ppduster::report::{self, OutputFormat};
use ppduster::rules::RulePack;
use ppduster::scan::{self, ScanOptions};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliOutput {
    Table,
    Json,
}

impl From<CliOutput> for OutputFormat {
    fn from(v: CliOutput) -> Self {
        match v {
            CliOutput::Table => OutputFormat::Table,
            CliOutput::Json => OutputFormat::Json,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "ppduster",
    version,
    about = "Safe junk cleaner: caches, logs, temp files, and leftovers",
    long_about = "ppduster scans known junk locations using versioned YAML rule packs.\n\
                  Default is always safe: dry-run, age filters, never-touch paths, trash delete.\n\
                  Inspired by lessons from BleachBit, CleanMyMac, CCleaner and 100+ OSS tools."
)]
struct Cli {
    /// Extra rules directory (in addition to bundled ./rules)
    #[arg(long, global = true)]
    rules_dir: Option<PathBuf>,

    /// Output format
    #[arg(long, short = 'o', global = true, value_enum, default_value = "table")]
    output: CliOutput,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Scan for junk without deleting anything
    Scan {
        /// Only these category ids (comma-separated or repeat flag)
        #[arg(long, short = 'c')]
        category: Vec<String>,

        /// Include disabled / high-risk rules
        #[arg(long)]
        all: bool,

        /// Override minimum file age in days (default: per-rule)
        #[arg(long)]
        min_age: Option<u64>,

        /// Max entries printed in table mode
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Delete junk found by the same scanner (dry-run unless --yes)
    Clean {
        #[arg(long, short = 'c')]
        category: Vec<String>,

        #[arg(long)]
        all: bool,

        #[arg(long)]
        min_age: Option<u64>,

        /// Actually delete (otherwise dry-run)
        #[arg(long)]
        yes: bool,

        /// Permanent delete instead of Trash/Recycle Bin
        #[arg(long)]
        permanent: bool,

        /// Require typing this exact string to confirm permanent mass delete
        #[arg(long, default_value = "DELETE")]
        confirm_phrase: String,

        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// List loaded rules
    Rules {
        #[command(subcommand)]
        action: RulesCmd,
    },
    /// List categories
    Categories {
        #[arg(long)]
        all: bool,
    },
    /// Automation tasks (clone repos, brew install, run commands, …)
    Automate {
        #[command(subcommand)]
        action: AutomateCmd,

        /// Extra automations directory (in addition to ./automations)
        #[arg(long, global = true)]
        automations_dir: Option<PathBuf>,
    },
    /// Environment and safety self-check
    Doctor,
}

#[derive(Subcommand, Debug)]
enum RulesCmd {
    List {
        #[arg(long)]
        all: bool,
    },
    Show {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum AutomateCmd {
    /// List all available automation tasks
    List,
    /// Show details and steps for a task
    Show {
        /// Task id (YAML file stem under automations/)
        id: String,
    },
    /// Preview or run an automation task
    ///
    /// Dry-run by default. Pass --yes to execute. Privileged steps
    /// (install_dmg, install_pkg) also require --allow-privileged.
    Run {
        /// Task id to run
        id: String,

        /// Actually execute steps (default: dry-run preview only)
        #[arg(long)]
        yes: bool,

        /// Allow steps that may require elevated privileges (install_dmg, install_pkg)
        #[arg(long)]
        allow_privileged: bool,

        /// [reserved] Trust gate for external task packs — not yet wired
        #[arg(long, hide = true)]
        trust_pack: Option<String>,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{} {:#}", "error:".red().bold(), err);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let pack = load_pack(cli.rules_dir.as_ref())?;
    let output = OutputFormat::from(cli.output);

    match cli.command {
        Commands::Scan {
            category,
            all,
            min_age,
            limit,
        } => {
            let opts = ScanOptions {
                categories: flatten_categories(category),
                include_disabled: all,
                min_age_override: min_age,
            };
            let report = scan::scan(&pack, &opts)?;
            report::print_scan(&report, output, limit)?;
        }
        Commands::Clean {
            category,
            all,
            min_age,
            yes,
            permanent,
            confirm_phrase,
            limit,
        } => {
            if permanent && yes {
                eprintln!(
                    "{}",
                    "WARNING: permanent delete requested (not Trash)."
                        .yellow()
                        .bold()
                );
            }
            let opts = ScanOptions {
                categories: flatten_categories(category),
                include_disabled: all,
                min_age_override: min_age,
            };
            let report = scan::scan(&pack, &opts)?;
            if !yes {
                eprintln!(
                    "{}",
                    "Dry-run: nothing deleted. Re-run with --yes to apply.".cyan()
                );
                report::print_scan(&report, output, limit)?;
                return Ok(());
            }
            if permanent {
                eprint!("Type {confirm_phrase} to permanently delete: ");
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .context("read confirmation")?;
                if line.trim() != confirm_phrase {
                    anyhow::bail!("confirmation phrase mismatch; aborting");
                }
            }
            let result = clean::clean(&report, permanent)?;
            report::print_clean(&result, output)?;
        }
        Commands::Rules { action } => match action {
            RulesCmd::List { all } => report::print_rules(&pack, all, output)?,
            RulesCmd::Show { id } => report::print_rule(&pack, &id, output)?,
        },
        Commands::Categories { all } => report::print_categories(&pack, all, output)?,
        Commands::Automate {
            action,
            automations_dir,
        } => {
            let dirs = load_automations_dirs(automations_dir.as_ref());
            run_automate(action, &dirs)?;
        }
        Commands::Doctor => report::print_doctor(&pack)?,
    }
    Ok(())
}

fn load_automations_dirs(extra: Option<&PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        let d = cwd.join("automations");
        if d.is_dir() {
            dirs.push(d);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let near = parent.join("automations");
            if near.is_dir() {
                dirs.push(near);
            }
            if let Some(root) = parent.parent().and_then(|p| p.parent()) {
                let dev = root.join("automations");
                if dev.is_dir() {
                    dirs.push(dev);
                }
            }
        }
    }
    if let Some(e) = extra {
        dirs.push(e.clone());
    }
    dirs
}

fn run_automate(action: AutomateCmd, dirs: &[PathBuf]) -> Result<()> {
    match action {
        AutomateCmd::List => {
            let tasks = automation::list_tasks(dirs)?;
            if tasks.is_empty() {
                eprintln!(
                    "{}",
                    "No automation tasks found. Create YAML files in ./automations/.".cyan()
                );
            } else {
                for t in &tasks {
                    println!("{:20} {} steps  {}", t.id.bold(), t.step_count, t.description);
                }
            }
        }
        AutomateCmd::Show { id } => {
            let task = automation::load_task(dirs, &id)?;
            println!("{}: {}", "Task".bold(), task.name);
            if !task.description.is_empty() {
                println!("{}: {}", "Description".bold(), task.description);
            }
            println!("{}: {}", "Steps".bold(), task.steps.len());
            for (i, step) in task.steps.iter().enumerate() {
                let label = step.label.as_deref().unwrap_or(step.kind.kind_label());
                let privileged = if step.kind.requires_privilege() {
                    " (requires privilege)".yellow().to_string()
                } else {
                    String::new()
                };
                let exec = if step.kind.is_arbitrary_execution() {
                    " (external execution)".yellow().to_string()
                } else {
                    String::new()
                };
                println!("  [{:>2}] {}{}{} — {}", i + 1, step.kind.kind_label().bold(), privileged, exec, label);
            }
        }
        AutomateCmd::Run {
            id,
            yes,
            allow_privileged,
            trust_pack: _,
        } => {
            let task = automation::load_task(dirs, &id)?;

            // Security gate: check for privileged steps
            let has_privileged = task.steps.iter().any(|s| s.kind.requires_privilege());
            if has_privileged && !allow_privileged {
                anyhow::bail!(
                    "task '{}' contains privileged steps (install_dmg, install_pkg).\n\
                     Re-run with --allow-privileged to acknowledge that these steps \
                     may require sudo or modify /Applications.",
                    id
                );
            }

            // Security gate: warn about arbitrary execution steps
            let has_exec = task.steps.iter().any(|s| s.kind.is_arbitrary_execution());
            if has_exec && yes {
                eprintln!(
                    "{}",
                    "WARNING: this task contains steps that run external commands \
                     (brew_install, clone_repo, run_command). Only proceed if you \
                     trust the source of this task file."
                        .yellow()
                        .bold()
                );
            }

            if !yes {
                // Dry-run: print preview, no execution.
                automation::preview_task(&task);
                return Ok(());
            }

            // Live execution — delegated to runner core module once merged.
            // Until then, surface a clear error so the CLI compiles and the
            // dry-run path is fully usable.
            eprintln!(
                "{}",
                "Live execution not yet available: waiting for automation runner core to merge."
                    .yellow()
            );
            eprintln!("Dry-run preview:");
            automation::preview_task(&task);
            anyhow::bail!(
                "runner not yet integrated; run without --yes to preview, \
                 or wait for the runner core module to land"
            );
        }
    }
    Ok(())
}

fn load_pack(extra: Option<&PathBuf>) -> Result<RulePack> {
    let mut dirs = Vec::new();
    let cwd_rules = std::env::current_dir()?.join("rules");
    if cwd_rules.is_dir() {
        dirs.push(cwd_rules);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let near = parent.join("rules");
            if near.is_dir() {
                dirs.push(near);
            }
            if let Some(root) = parent.parent().and_then(|p| p.parent()) {
                let dev = root.join("rules");
                if dev.is_dir() {
                    dirs.push(dev);
                }
            }
        }
    }
    if let Some(extra) = extra {
        dirs.push(extra.clone());
    }
    if dirs.is_empty() {
        anyhow::bail!(
            "no rules directory found; create ./rules or pass --rules-dir. \
             Run from the ppduster repo root or install rule packs."
        );
    }
    RulePack::load_many(&dirs)
}

fn flatten_categories(raw: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in raw {
        for part in item.split(',') {
            let t = part.trim();
            if !t.is_empty() {
                out.push(t.to_string());
            }
        }
    }
    out
}
