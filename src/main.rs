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
    /// Automation packs (setup tasks: clone repos, brew install, run commands, …)
    ///
    /// Alias: `automate` also accepted for backwards compat.
    #[command(alias = "automate")]
    Setup {
        #[command(subcommand)]
        action: SetupCmd,

        /// Extra automations directory (in addition to ./automations)
        #[arg(long, global = true)]
        automations_dir: Option<PathBuf>,

        /// Trust level for packs loaded from the extra directory
        /// (bundled | user | external; default: user)
        #[arg(long, global = true, value_enum, default_value = "user")]
        trust_pack: CliPackTrust,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPackTrust {
    Bundled,
    User,
    External,
}

impl From<CliPackTrust> for automation::PackTrust {
    fn from(v: CliPackTrust) -> Self {
        match v {
            CliPackTrust::Bundled => automation::PackTrust::Bundled,
            CliPackTrust::User => automation::PackTrust::User,
            CliPackTrust::External => automation::PackTrust::External,
        }
    }
}

#[derive(Subcommand, Debug)]
enum SetupCmd {
    /// List all available automation packs
    List,
    /// Show details and steps for a pack
    Show {
        /// Pack id (the `pack:` field in the YAML, usually the file stem)
        id: String,
    },
    /// Preview or run an automation pack
    ///
    /// Dry-run by default — nothing executes until --yes is passed.
    /// Packs are security-validated against their trust level before display.
    Run {
        /// Pack id to run
        id: String,

        /// Actually execute steps (default: dry-run preview only)
        #[arg(long)]
        yes: bool,

        /// Allow steps that may require elevated privileges (install-dmg, install-pkg)
        #[arg(long)]
        allow_privileged: bool,
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
        Commands::Setup {
            action,
            automations_dir,
            trust_pack,
        } => {
            let dirs = load_automations_dirs(automations_dir.as_ref());
            let trust = automation::PackTrust::from(trust_pack);
            run_setup(action, &dirs, trust)?;
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

fn run_setup(action: SetupCmd, dirs: &[PathBuf], trust: automation::PackTrust) -> Result<()> {
    use automation::AutomationStep;

    match action {
        SetupCmd::List => {
            let packs = automation::AutomationPack::load_many_with_trust(dirs, trust)?;
            if packs.is_empty() {
                eprintln!(
                    "{}",
                    "No automation packs found. Create YAML files in ./automations/.".cyan()
                );
            } else {
                for p in &packs {
                    println!(
                        "{:20} {:>2} step(s)  {}",
                        p.pack.bold(),
                        p.steps.len(),
                        p.description
                    );
                }
            }
        }
        SetupCmd::Show { id } => {
            let packs = automation::AutomationPack::load_many_with_trust(dirs, trust)?;
            let pack = packs
                .into_iter()
                .find(|p| p.pack == id)
                .ok_or_else(|| anyhow::anyhow!("pack '{}' not found in {:?}", id, dirs))?;

            // Validate security constraints before display.
            pack.validate()?;

            println!("{}: {}", "Pack".bold(), pack.pack);
            if !pack.description.is_empty() {
                println!("{}: {}", "Description".bold(), pack.description);
            }
            println!("{}: {}", "Platform".bold(), pack.platform.as_str());
            println!("{}: {:?}", "Trust".bold(), pack.trust);
            println!("{}: {}", "Steps".bold(), pack.steps.len());
            for (i, step) in pack.applicable_steps().iter().enumerate() {
                let privileged = matches!(
                    step,
                    AutomationStep::InstallDmg(_) | AutomationStep::InstallPkg(_)
                );
                let exec = matches!(
                    step,
                    AutomationStep::RunCommand(_)
                        | AutomationStep::GitClone(_)
                        | AutomationStep::BrewInstall(_)
                        | AutomationStep::BrewCask(_)
                );
                let priv_tag = if privileged {
                    format!(" {}", "(requires privilege)".yellow())
                } else {
                    String::new()
                };
                let exec_tag = if exec {
                    format!(" {}", "(external execution)".yellow())
                } else {
                    String::new()
                };
                println!(
                    "  [{:>2}] {}{}{}", i + 1,
                    step.kind_label().bold(),
                    priv_tag,
                    exec_tag
                );
            }
        }
        SetupCmd::Run {
            id,
            yes,
            allow_privileged,
        } => {
            let packs = automation::AutomationPack::load_many_with_trust(dirs, trust)?;
            let pack = packs
                .into_iter()
                .find(|p| p.pack == id)
                .ok_or_else(|| anyhow::anyhow!("pack '{}' not found in {:?}", id, dirs))?;

            // Security validation against trust level (sha256, write-dest, shell_expand).
            pack.validate()?;

            // Security gate: privileged steps require explicit opt-in.
            let has_privileged = pack.applicable_steps().iter().any(|s| {
                matches!(s, AutomationStep::InstallDmg(_) | AutomationStep::InstallPkg(_))
            });
            if has_privileged && !allow_privileged {
                anyhow::bail!(
                    "pack '{}' contains privileged steps (install-dmg, install-pkg).\n\
                     Re-run with --allow-privileged to acknowledge these steps \
                     may require elevated permissions or modify /Applications.",
                    id
                );
            }

            // Security gate: warn about arbitrary execution when --yes is set.
            let has_exec = pack.applicable_steps().iter().any(|s| {
                matches!(
                    s,
                    AutomationStep::RunCommand(_)
                        | AutomationStep::GitClone(_)
                        | AutomationStep::BrewInstall(_)
                        | AutomationStep::BrewCask(_)
                )
            });
            if has_exec && yes {
                eprintln!(
                    "{}",
                    "WARNING: this pack contains steps that run external commands. \
                     Only proceed if you trust the source of this task file."
                        .yellow()
                        .bold()
                );
            }

            if !yes {
                // Dry-run: print preview, nothing executes.
                print_pack_preview(&pack);
                return Ok(());
            }

            // Live execution is delegated to the runner core module.
            // Until that module merges, surface a clear, actionable error.
            eprintln!(
                "{}",
                "Live execution not yet available — waiting for automation runner core to merge."
                    .yellow()
            );
            eprintln!("Dry-run preview:");
            print_pack_preview(&pack);
            anyhow::bail!(
                "runner not yet integrated; re-run without --yes to preview, \
                 or wait for the runner core module to land"
            );
        }
    }
    Ok(())
}

/// Print a human-readable dry-run preview of a pack's steps.
fn print_pack_preview(pack: &automation::AutomationPack) {
    use automation::AutomationStep;

    println!("Pack:  {}", pack.pack.bold());
    if !pack.description.is_empty() {
        println!("       {}", pack.description);
    }
    println!();
    let steps = pack.applicable_steps();
    if steps.is_empty() {
        println!("  (no applicable steps for this platform)");
    }
    for (i, step) in steps.iter().enumerate() {
        print!("  [{:>2}] {}", i + 1, step.kind_label().bold());
        match step {
            AutomationStep::BrewInstall(s) => {
                let tap = s.tap.as_deref().map(|t| format!(" (tap: {t})")).unwrap_or_default();
                println!(" — brew install {}{}", s.package, tap);
            }
            AutomationStep::BrewCask(s) => println!(" — brew install --cask {}", s.package),
            AutomationStep::GitClone(s) => {
                let depth = if s.depth > 0 { format!(" --depth {}", s.depth) } else { String::new() };
                let branch = s.branch.as_deref().map(|b| format!(" -b {b}")).unwrap_or_default();
                println!(" — git clone{depth}{branch} {} {}", s.url, s.dest);
            }
            AutomationStep::RunCommand(s) => {
                let cwd = s.working_dir.as_deref().map(|d| format!(" (cwd: {d})")).unwrap_or_default();
                println!(" — {}{}", s.argv.join(" "), cwd);
            }
            AutomationStep::Download(s) => {
                let sha = s.sha256.as_deref().map(|h| format!(" sha256:{h}")).unwrap_or_default();
                println!(" — {} → {}{}", s.url, s.dest, sha);
            }
            AutomationStep::Extract(s) => println!(" — {} → {}", s.src, s.dest),
            AutomationStep::InstallDmg(s) => println!(" — mount {} → copy {} to {}", s.src, s.app_name, s.dest_dir),
            AutomationStep::InstallPkg(s) => {
                println!(" — installer -pkg {} -target {}", s.src, s.target);
            }
            AutomationStep::Symlink(s) => println!(" — ln -s{} {} {}", if s.force { "f" } else { "" }, s.src, s.dest),
            AutomationStep::WriteFile(s) => println!(" — write {} ({} bytes)", s.dest, s.content.len()),
            AutomationStep::SetEnvHint(s) => println!(" — export {}={} (hint)", s.var, s.value),
        }
    }
    println!();
    println!("{}", "(dry-run — no changes made; re-run with --yes to execute)".cyan());
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
