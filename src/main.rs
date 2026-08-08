use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use ppduster::audit;
use ppduster::automation::{run_task, PackTrust, RunOptions, TaskPack, TaskSource};
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

    /// Allow loading automation task packs from external directories.
    #[arg(long, global = true)]
    trust_external_packs: bool,

    /// Write CLI activity to a JSONL audit log (defaults to ~/.local/share/ppduster/audit.log)
    #[arg(long, global = true)]
    audit_log: Option<PathBuf>,

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
    /// Environment and safety self-check
    Doctor,
    /// Safe-by-default setup automation tasks
    Setup {
        #[command(subcommand)]
        action: SetupCmd,
    },
    /// Show recent audit entries
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
enum SetupCmd {
    List,
    Show {
        id: String,
    },
    Run {
        id: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        allow_shell: bool,
        #[arg(long)]
        allow_elevation: bool,
        #[arg(long)]
        tasks_dir: Vec<PathBuf>,
    },
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

fn main() {
    if let Err(err) = run() {
        eprintln!("{} {:#}", "error:".red().bold(), err);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let rule_dirs = discover_rule_dirs(cli.rules_dir.as_ref())?;
    let pack = load_pack_from_dirs(&rule_dirs)?;
    let output = OutputFormat::from(cli.output);
    let audit_path = audit::resolve_log_path(cli.audit_log.as_ref());
    let log_audit = |action: &str, outcome: &str, detail: Option<&str>| {
        if let Some(path) = audit_path.as_ref() {
            let _ = audit::append_event(path, action, outcome, detail);
        }
    };

    let result: Result<()> = match cli.command {
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
            Ok(())
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
            Ok(())
        }
        Commands::Rules { action } => match action {
            RulesCmd::List { all } => {
                report::print_rules(&pack, all, output)?;
                Ok(())
            }
            RulesCmd::Show { id } => {
                report::print_rule(&pack, &id, output)?;
                Ok(())
            }
        },
        Commands::Categories { all } => {
            report::print_categories(&pack, all, output)?;
            Ok(())
        }
        Commands::Doctor => {
            report::print_doctor(&pack, &rule_dirs, output)?;
            Ok(())
        }
        Commands::Setup { action } => {
            let tasks = load_tasks(&action, cli.trust_external_packs)?;
            match action {
                SetupCmd::List => {
                    for task in &tasks.tasks {
                        println!("{}\t{}", task.id, task.name);
                    }
                    Ok(())
                }
                SetupCmd::Show { id } => {
                    let task = tasks
                        .get(&id)
                        .ok_or_else(|| anyhow::anyhow!("unknown task id {}", id))?;
                    println!("{}", serde_yaml::to_string(task)?);
                    Ok(())
                }
                SetupCmd::Run {
                    id,
                    yes,
                    allow_shell,
                    allow_elevation,
                    ..
                } => {
                    let task = tasks
                        .get(&id)
                        .ok_or_else(|| anyhow::anyhow!("unknown task id {}", id))?;
                    let report = run_task(
                        task,
                        &RunOptions {
                            apply: yes,
                            allow_shell,
                            allow_elevation,
                        },
                    )?;
                    report::print_setup(&report, output)?;
                    Ok(())
                }
            }
        }
        Commands::Audit { limit } => {
            let path = audit::resolve_log_path(cli.audit_log.as_ref()).unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(".ppduster-audit.log")
            });
            let entries = audit::read_events(&path)?;
            if entries.is_empty() {
                println!("No audit entries found.");
            } else {
                for entry in entries.iter().rev().take(limit) {
                    println!(
                        "{} {} {}{}",
                        entry.timestamp,
                        entry.action,
                        entry.outcome,
                        entry
                            .detail
                            .as_deref()
                            .map(|detail| format!(" :: {detail}"))
                            .unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
    };

    match result {
        Ok(()) => {
            log_audit("command", "completed", Some("cli command completed"));
            Ok(())
        }
        Err(err) => {
            log_audit("command", "failed", Some(&err.to_string()));
            Err(err)
        }
    }
}

fn discover_rule_dirs(extra: Option<&PathBuf>) -> Result<Vec<PathBuf>> {
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

    let mut unique_dirs = Vec::new();
    for dir in dirs {
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if unique_dirs.iter().any(|d| d == &canon) {
            continue;
        }
        unique_dirs.push(canon);
    }
    Ok(unique_dirs)
}

fn load_pack_from_dirs(rule_dirs: &[PathBuf]) -> Result<RulePack> {
    if rule_dirs.is_empty() {
        anyhow::bail!(
            "no rules directory found; create ./rules or pass --rules-dir. \
             Run from the ppduster repo root or install rule packs."
        );
    }
    RulePack::load_many(rule_dirs)
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

fn load_tasks(action: &SetupCmd, trust_external_packs: bool) -> Result<TaskPack> {
    let mut sources = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let near = parent.join("tasks");
            if near.is_dir() {
                sources.push(TaskSource {
                    path: near,
                    trust: PackTrust::Bundled,
                });
            }
            if cfg!(debug_assertions)
                && parent.file_name().and_then(|n| n.to_str()) == Some("debug")
                && parent
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some("target")
            {
                if let Some(root) = parent.parent().and_then(|p| p.parent()) {
                    let dev = root.join("tasks");
                    if dev.is_dir() {
                        sources.push(TaskSource {
                            path: dev,
                            trust: PackTrust::Bundled,
                        });
                    }
                }
            }
        }
    }
    if let SetupCmd::Run { tasks_dir, .. } = action {
        for dir in tasks_dir {
            sources.push(TaskSource {
                path: dir.clone(),
                trust: PackTrust::External,
            });
        }
    }
    if sources.is_empty() {
        anyhow::bail!(
            "no tasks directory found; install bundled tasks near the binary or pass --tasks-dir with --trust-external-packs"
        );
    }
    TaskPack::load_many(&sources, trust_external_packs)
}
