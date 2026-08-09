use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use ppduster::audit;
use ppduster::automation::package_secrets::{
    exec_for_task as exec_with_package_secrets, init_for_task as init_package_secrets,
    vault_path_for_task, PackageTool, PasswordMode, SecretInitMode,
};
use ppduster::automation::{run_task, PackTrust, ReleaseChannel, RunOptions, TaskPack, TaskSource};
use ppduster::clean;
use ppduster::report::{self, OutputFormat};
use ppduster::rules::RulePack;
use ppduster::scan::{self, ScanOptions};
use std::cell::Cell;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliOutput {
    Table,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliReleaseChannel {
    Release,
    Beta,
}

impl From<CliReleaseChannel> for ReleaseChannel {
    fn from(value: CliReleaseChannel) -> Self {
        match value {
            CliReleaseChannel::Release => Self::Release,
            CliReleaseChannel::Beta => Self::Beta,
        }
    }
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
        /// Override the release channel for tasks that support it
        #[arg(long, value_enum)]
        channel: Option<CliReleaseChannel>,
        #[arg(long)]
        tasks_dir: Vec<PathBuf>,
    },
    /// Create or use the password-encrypted package credential vault.
    Secrets {
        #[command(subcommand)]
        action: PackageSecretsCmd,
    },
}

#[derive(Subcommand, Debug)]
enum PackageSecretsCmd {
    /// Create a new encrypted vault outside the repository.
    Init {
        /// Bundled package-registry task id.
        id: String,
        /// Override the vault path (primarily for isolated automation).
        #[arg(long)]
        file: Option<PathBuf>,
        /// Read username, token, password, and confirmation as JSON from stdin.
        #[arg(long)]
        input_json_stdin: bool,
    },
    /// Unlock the vault and run a supported package command without a shell.
    Exec {
        /// Bundled package-registry task id.
        id: String,
        #[arg(value_enum)]
        tool: PackageToolCli,
        /// Override the vault path (primarily for isolated automation).
        #[arg(long)]
        file: Option<PathBuf>,
        /// Read one password line from stdin instead of prompting on a terminal.
        #[arg(long)]
        password_stdin: bool,
        /// Package-manager arguments. A literal `--` separator is required.
        #[arg(last = true, required = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PackageToolCli {
    Npm,
    Dotnet,
}

impl From<PackageToolCli> for PackageTool {
    fn from(value: PackageToolCli) -> Self {
        match value {
            PackageToolCli::Npm => Self::Npm,
            PackageToolCli::Dotnet => Self::Dotnet,
        }
    }
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
    let output = OutputFormat::from(cli.output);
    let audit_path = audit::resolve_log_path(cli.audit_log.as_ref());
    let suppress_audit = Cell::new(false);
    let log_audit = |action: &str, outcome: &str, detail: Option<&str>| {
        if !suppress_audit.get() {
            if let Some(path) = audit_path.as_ref() {
                let _ = audit::append_event(path, action, outcome, detail);
            }
        }
    };

    let result: Result<()> = (|| match cli.command {
        Commands::Scan {
            category,
            all,
            min_age,
            limit,
        } => {
            let pack = load_pack_from_dirs(&rule_dirs)?;
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
            let pack = load_pack_from_dirs(&rule_dirs)?;
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
                let pack = load_pack_from_dirs(&rule_dirs)?;
                report::print_rules(&pack, all, output)?;
                Ok(())
            }
            RulesCmd::Show { id } => {
                let pack = load_pack_from_dirs(&rule_dirs)?;
                report::print_rule(&pack, &id, output)?;
                Ok(())
            }
        },
        Commands::Categories { all } => {
            let pack = load_pack_from_dirs(&rule_dirs)?;
            report::print_categories(&pack, all, output)?;
            Ok(())
        }
        Commands::Doctor => {
            let pack = load_pack_from_dirs(&rule_dirs)?;
            report::print_doctor(&pack, &rule_dirs, output)?;
            Ok(())
        }
        Commands::Setup { action } => match action {
            SetupCmd::List => {
                let tasks = load_tasks(&[], cli.trust_external_packs)?;
                for task in &tasks.tasks {
                    let kind = if task.is_template() {
                        "template"
                    } else {
                        "scenario"
                    };
                    let description = task
                        .description
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("{}\t{}\t{}\t{}", task.id, kind, task.name, description);
                }
                Ok(())
            }
            SetupCmd::Show { id } => {
                let tasks = load_tasks(&[], cli.trust_external_packs)?;
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
                channel,
                tasks_dir,
            } => {
                let tasks = load_tasks(&tasks_dir, cli.trust_external_packs)?;
                let task = tasks.resolve(&id)?;
                let report = run_task(
                    &task,
                    &RunOptions {
                        apply: yes,
                        allow_shell,
                        allow_elevation,
                        release_channel: channel.map(Into::into),
                    },
                )?;
                report::print_setup(&report, output)?;
                if report.errors.is_empty() {
                    Ok(())
                } else {
                    anyhow::bail!(
                        "setup task {} failed in {} step(s)",
                        report.task_id,
                        report.errors.len()
                    )
                }
            }
            SetupCmd::Secrets { action } => {
                let tasks = load_tasks(&[], false)?;
                match action {
                    PackageSecretsCmd::Init {
                        id,
                        file,
                        input_json_stdin,
                    } => {
                        let task = tasks
                            .get(&id)
                            .ok_or_else(|| anyhow::anyhow!("unknown bundled task id {}", id))?;
                        let vault_path = vault_path_for_task(task, file.as_deref())?;
                        if audit_path
                            .as_deref()
                            .is_some_and(|audit| paths_collide(audit, &vault_path))
                        {
                            suppress_audit.set(true);
                            anyhow::bail!("audit log path must differ from the encrypted vault");
                        }
                        let mode = if input_json_stdin {
                            SecretInitMode::JsonStdin
                        } else {
                            SecretInitMode::Interactive
                        };
                        let path = init_package_secrets(task, file.as_deref(), mode)?;
                        println!("Created encrypted package secret vault: {}", path.display());
                        Ok(())
                    }
                    PackageSecretsCmd::Exec {
                        id,
                        tool,
                        file,
                        password_stdin,
                        args,
                    } => {
                        let task = tasks
                            .get(&id)
                            .ok_or_else(|| anyhow::anyhow!("unknown bundled task id {}", id))?;
                        let vault_path = vault_path_for_task(task, file.as_deref())?;
                        if audit_path
                            .as_deref()
                            .is_some_and(|audit| paths_collide(audit, &vault_path))
                        {
                            suppress_audit.set(true);
                            anyhow::bail!("audit log path must differ from the encrypted vault");
                        }
                        let password_mode = if password_stdin {
                            PasswordMode::Stdin
                        } else {
                            PasswordMode::Interactive
                        };
                        let tool = PackageTool::from(tool);
                        let status = exec_with_package_secrets(
                            task,
                            file.as_deref(),
                            password_mode,
                            tool,
                            &args,
                        )?;
                        if status.success() {
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!(
                                "{} package command exited with status {}",
                                match tool {
                                    PackageTool::Npm => "npm",
                                    PackageTool::Dotnet => "dotnet",
                                },
                                status
                            ))
                        }
                    }
                }
            }
        },
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
    })();

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

fn paths_collide(left: &std::path::Path, right: &std::path::Path) -> bool {
    if let Ok(metadata) = std::fs::symlink_metadata(left) {
        if metadata.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return true;
            }
        }
    }
    if left.exists() && right.exists() && same_file::is_same_file(left, right).unwrap_or(false) {
        return true;
    }
    let left = normalized_path(left);
    let right = normalized_path(right);
    #[cfg(any(target_os = "macos", windows))]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        left == right
    }
}

fn normalized_path(path: &std::path::Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
        if let Ok(parent) = parent.canonicalize() {
            return parent.join(name);
        }
    }
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
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

fn load_tasks(tasks_dirs: &[PathBuf], trust_external_packs: bool) -> Result<TaskPack> {
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
    for dir in tasks_dirs {
        sources.push(TaskSource {
            path: dir.clone(),
            trust: PackTrust::External,
        });
    }
    if sources.is_empty() {
        anyhow::bail!(
            "no tasks directory found; install bundled tasks near the binary or pass --tasks-dir with --trust-external-packs"
        );
    }
    TaskPack::load_many(&sources, trust_external_packs)
}
