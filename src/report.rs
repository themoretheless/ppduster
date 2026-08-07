use crate::automation::{RunReport, StepStatus};
use crate::clean::CleanResult;
use crate::rules::{host_platform_name, RulePack};
use crate::scan::ScanReport;
use anyhow::{Context, Result};
use bytesize::ByteSize;
use colored::Colorize;
use serde::Serialize;
use tabled::{
    settings::{object::Columns, Modify, Style, Width},
    Table, Tabled,
};

#[derive(Debug, Clone, Copy)]
pub enum OutputFormat {
    Table,
    Json,
}

pub fn print_scan(report: &ScanReport, output: OutputFormat, limit: usize) -> Result<()> {
    match output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
        }
        OutputFormat::Table => {
            println!(
                "{} {} in {} finding(s) across {}/{} rules",
                "Found".green().bold(),
                ByteSize::b(report.total_bytes).to_string().bold(),
                report.findings.len(),
                report.rules_matched,
                report.rules_scanned
            );
            if report.findings.is_empty() {
                println!("{}", "No junk matched current rules/filters.".dimmed());
                return Ok(());
            }

            #[derive(Tabled)]
            struct Row {
                category: String,
                risk: String,
                size: String,
                path: String,
                rule: String,
            }

            let rows: Vec<Row> = report
                .findings
                .iter()
                .take(limit)
                .map(|f| Row {
                    category: f.category.clone(),
                    risk: f.risk.clone(),
                    size: ByteSize::b(f.bytes).to_string(),
                    path: f.path.display().to_string(),
                    rule: f.rule_id.clone(),
                })
                .collect();

            let mut table = Table::new(rows);
            table.with(Style::rounded());
            table.with(Modify::new(Columns::single(3)).with(Width::wrap(72).keep_words(true)));
            println!("{table}");

            if report.findings.len() > limit {
                println!(
                    "{}",
                    format!(
                        "... and {} more (raise --limit or use -o json)",
                        report.findings.len() - limit
                    )
                    .dimmed()
                );
            }

            let mut by_cat: std::collections::BTreeMap<String, (u64, usize)> =
                std::collections::BTreeMap::new();
            for f in &report.findings {
                let e = by_cat.entry(f.category.clone()).or_insert((0, 0));
                e.0 = e.0.saturating_add(f.bytes);
                e.1 += 1;
            }
            println!("\n{}", "By category:".bold());
            for (cat, (bytes, count)) in by_cat {
                println!(
                    "  {:<20} {:>10}  ({} items)",
                    cat,
                    ByteSize::b(bytes).to_string(),
                    count
                );
            }

            if !report.skipped_roots.is_empty() {
                println!("\n{}", "Skipped roots:".yellow());
                for s in report.skipped_roots.iter().take(20) {
                    println!("  - {s}");
                }
            }
            println!(
                "\n{}",
                "Nothing was deleted. Use `ppduster clean --yes` after review.".cyan()
            );
        }
    }
    Ok(())
}

pub fn print_clean(result: &CleanResult, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(result)?);
        }
        OutputFormat::Table => {
            println!(
                "{} {} item(s), freed ~{}",
                "Deleted".green().bold(),
                result.deleted.len(),
                ByteSize::b(result.freed_bytes)
            );
            println!(
                "method={}  report_only_skipped={}",
                if result.permanent {
                    "permanent"
                } else {
                    "trash"
                },
                result.skipped_report_only
            );
            for item in result.deleted.iter().take(50) {
                println!(
                    "  [{}] {} ({})",
                    item.method,
                    item.path.display(),
                    ByteSize::b(item.bytes)
                );
            }
            if result.deleted.len() > 50 {
                println!("  ... {} more", result.deleted.len() - 50);
            }
            if !result.errors.is_empty() {
                println!("{}", "Errors:".red().bold());
                for e in &result.errors {
                    println!("  - {e}");
                }
            }
        }
    }
    Ok(())
}

pub fn print_rules(pack: &RulePack, all: bool, output: OutputFormat) -> Result<()> {
    #[derive(Serialize)]
    struct RuleRow<'a> {
        id: &'a str,
        name: &'a str,
        category: &'a str,
        platform: &'a str,
        risk: &'a str,
        enabled: bool,
        min_age_days: u64,
    }
    let rows: Vec<RuleRow> = pack
        .rules
        .iter()
        .filter(|r| r.platform.matches_host() || all)
        .filter(|r| all || r.default_enabled)
        .map(|r| RuleRow {
            id: &r.id,
            name: &r.name,
            category: &r.category,
            platform: r.platform.as_str(),
            risk: r.risk.as_str(),
            enabled: r.default_enabled,
            min_age_days: r.min_age_days,
        })
        .collect();

    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&rows)?),
        OutputFormat::Table => {
            #[derive(Tabled)]
            struct T {
                id: String,
                category: String,
                risk: String,
                age_d: u64,
                enabled: bool,
                name: String,
            }
            let trows: Vec<T> = rows
                .iter()
                .map(|r| T {
                    id: r.id.to_string(),
                    category: r.category.to_string(),
                    risk: r.risk.to_string(),
                    age_d: r.min_age_days,
                    enabled: r.enabled,
                    name: r.name.to_string(),
                })
                .collect();
            let mut table = Table::new(trows);
            table.with(Style::rounded());
            println!("{table}");
            println!(
                "{} rule(s) from {} file(s)",
                rows.len(),
                pack.sources.len()
            );
        }
    }
    Ok(())
}

pub fn print_rule(pack: &RulePack, id: &str, output: OutputFormat) -> Result<()> {
    let rule = pack
        .get(id)
        .with_context(|| format!("unknown rule id '{id}'"))?;
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(rule)?),
        OutputFormat::Table => {
            println!("{} {}", "id:".bold(), rule.id);
            println!("{} {}", "name:".bold(), rule.name);
            println!("{} {}", "category:".bold(), rule.category);
            println!("{} {}", "platform:".bold(), rule.platform.as_str());
            println!("{} {}", "risk:".bold(), rule.risk.as_str());
            println!("{} {}", "enabled:".bold(), rule.default_enabled);
            println!("{} {}", "min_age_days:".bold(), rule.min_age_days);
            println!("{} {}", "description:".bold(), rule.description);
            println!("{}", "paths:".bold());
            for p in &rule.paths {
                println!("  - {p}");
            }
            if !rule.exclude_globs.is_empty() {
                println!("{}", "exclude:".bold());
                for g in &rule.exclude_globs {
                    println!("  - {g}");
                }
            }
        }
    }
    Ok(())
}

pub fn print_categories(pack: &RulePack, all: bool, output: OutputFormat) -> Result<()> {
    let cats = pack.categories(all);
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&cats)?),
        OutputFormat::Table => {
            println!("{} on {}", "Categories".bold(), host_platform_name());
            for (k, n) in cats {
                println!("  {k:<24} {n} rule(s)");
            }
        }
    }
    Ok(())
}

pub fn print_doctor(pack: &RulePack) -> Result<()> {
    println!("{}", "ppduster doctor".bold());
    println!("  platform:     {}", host_platform_name());
    println!(
        "  home:         {}",
        dirs::home_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    println!(
        "  cache_dir:    {}",
        dirs::cache_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into())
    );
    println!("  temp_dir:     {}", std::env::temp_dir().display());
    println!("  rules loaded: {}", pack.rules.len());
    println!("  rule files:   {}", pack.sources.len());
    for s in &pack.sources {
        println!("    - {}", s.display());
    }
    let active = pack.active_rules(&[], false).len();
    println!("  active rules: {active} (default_enabled on this OS)");
    println!("  safety:       dry-run default, trash delete, never-touch guards, age filters");
    println!("{}", "ok".green().bold());
    Ok(())
}

pub fn print_setup(report: &RunReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
        }
        OutputFormat::Table => {
            println!("{} {}", "setup task:".bold(), report.task_id);
            #[derive(Tabled)]
            struct Row {
                step: String,
                status: String,
                summary: String,
            }

            let rows: Vec<Row> = report
                .steps
                .iter()
                .map(|step| Row {
                    step: step.step_name.clone(),
                    status: render_step_status(&step.status),
                    summary: step.summary.clone(),
                })
                .collect();

            let mut table = Table::new(rows);
            table.with(Style::rounded());
            table.with(Modify::new(Columns::single(2)).with(Width::wrap(72).keep_words(true)));
            println!("{table}");

            println!("\n{}", "Logs:".bold());
            for step in &report.steps {
                println!("  {} [{}]", step.step_name, render_step_status(&step.status));
                for prerequisite in &step.prerequisites {
                    println!("    prerequisite: {prerequisite}");
                }
                for log in &step.logs {
                    println!("    - {}", log.message);
                }
            }
            if !report.errors.is_empty() {
                println!("\n{}", "Errors:".red().bold());
                for error in &report.errors {
                    println!("  - {error}");
                }
            }
        }
    }
    Ok(())
}

fn render_step_status(status: &StepStatus) -> String {
    match status {
        StepStatus::Pending => "pending".into(),
        StepStatus::Running => "running".yellow().bold().to_string(),
        StepStatus::WaitingForAttention => "waiting".red().bold().to_string(),
        StepStatus::Satisfied => "satisfied".green().to_string(),
        StepStatus::Applied => "applied".green().bold().to_string(),
        StepStatus::Failed => "failed".red().bold().to_string(),
    }
}
