use anyhow::Result;
use clap::{Parser, Subcommand};
use ppstore::app_store_installer::StoreOperation;
use ppstore::{app_store, app_store_cli, OutputFormat};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "ppstore",
    version,
    about = "Search, inspect, install, and update Mac App Store applications"
)]
struct Cli {
    /// Output format
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Table, global = true)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Search the Mac App Store catalog
    Search {
        /// Search terms (all words are joined into one query)
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Two-letter App Store country code
        #[arg(long)]
        country: Option<String>,
        /// Maximum number of catalog results (1-200)
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// List applications installed from the Mac App Store
    #[command(alias = "installed")]
    List {
        /// Scan an additional applications directory
        #[arg(long = "app-root")]
        app_roots: Vec<PathBuf>,
    },
    /// List potential updates for installed App Store applications
    Outdated {
        /// Two-letter App Store country code
        #[arg(long)]
        country: Option<String>,
        /// Scan an additional applications directory
        #[arg(long = "app-root")]
        app_roots: Vec<PathBuf>,
    },
    /// Install applications by numeric App Store (Adam) ID
    Install {
        #[arg(required = true, num_args = 1..)]
        app_ids: Vec<u64>,
        /// Two-letter App Store country code
        #[arg(long)]
        country: Option<String>,
        /// Obtain free apps; paid purchases must be completed in App Store
        #[arg(long)]
        get: bool,
        /// Apply the installation plan (without this flag the command is dry-run)
        #[arg(long)]
        yes: bool,
        /// Return after submission instead of verifying receipt/version
        #[arg(long)]
        no_wait: bool,
        /// Maximum seconds to wait for receipt/version verification
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
    /// Update installed App Store applications with potential updates
    #[command(alias = "update")]
    Upgrade {
        /// Optional numeric IDs; omit to update every compatible candidate
        app_ids: Vec<u64>,
        /// Two-letter App Store country code
        #[arg(long)]
        country: Option<String>,
        /// Apply the update plan (without this flag the command is dry-run)
        #[arg(long)]
        yes: bool,
        /// Return after submission instead of verifying receipt/version
        #[arg(long)]
        no_wait: bool,
        /// Maximum seconds to wait per application
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
    },
    /// Check local inventory and native installer prerequisites
    Doctor {
        /// Scan an additional applications directory
        #[arg(long = "app-root")]
        app_roots: Vec<PathBuf>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Search {
            query,
            country,
            limit,
        } => {
            let country = app_store_cli::resolve_country(country.as_deref())?;
            let report = app_store::search(&query.join(" "), &country, limit)?;
            app_store_cli::print_search(&report, cli.output)
        }
        Command::List { app_roots } => {
            let report = app_store::scan_installed(&app_roots)?;
            app_store_cli::print_installed(&report, cli.output)
        }
        Command::Outdated { country, app_roots } => {
            let country = app_store_cli::resolve_country(country.as_deref())?;
            let inventory = app_store::scan_installed(&app_roots)?;
            let mut report = app_store::check_updates(&inventory.apps, &country)?;
            report.warnings.extend(inventory.warnings);
            app_store_cli::print_updates(&report, cli.output)
        }
        Command::Install {
            app_ids,
            country,
            get,
            yes,
            no_wait,
            timeout,
        } => {
            let country = app_store_cli::resolve_country(country.as_deref())?;
            let operation = if get {
                StoreOperation::Get
            } else {
                StoreOperation::Install
            };
            let report = app_store_cli::install_apps(
                &app_ids,
                &country,
                operation,
                yes,
                !no_wait,
                Duration::from_secs(timeout),
            );
            app_store_cli::print_mutation(&report, cli.output)?;
            if report.has_failures() {
                anyhow::bail!(
                    "{} App Store installation request(s) failed",
                    report.failed_count()
                );
            }
            Ok(())
        }
        Command::Upgrade {
            app_ids,
            country,
            yes,
            no_wait,
            timeout,
        } => {
            let country = app_store_cli::resolve_country(country.as_deref())?;
            let selected = (!app_ids.is_empty()).then_some(app_ids.as_slice());
            let report = app_store_cli::upgrade_apps(
                selected,
                &country,
                yes,
                !no_wait,
                Duration::from_secs(timeout),
            );
            app_store_cli::print_mutation(&report, cli.output)?;
            if report.has_failures() {
                anyhow::bail!(
                    "{} App Store update request(s) failed",
                    report.failed_count()
                );
            }
            Ok(())
        }
        Command::Doctor { app_roots } => {
            let report = app_store_cli::doctor(&app_roots);
            app_store_cli::print_doctor(&report, cli.output)?;
            if !report.is_healthy() {
                anyhow::bail!("Mac App Store integration is not healthy");
            }
            Ok(())
        }
    }
}
