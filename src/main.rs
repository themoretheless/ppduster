use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
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
        Commands::Doctor => report::print_doctor(&pack)?,
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
