use crate::rules::{expand_path_template, DeleteMode, Risk, Rule, RulePack};
use crate::safety::{is_never_touch, is_old_enough, is_safe_rule_root, stays_under_root};
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub categories: Vec<String>,
    pub include_disabled: bool,
    pub min_age_override: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule_id: String,
    pub category: String,
    pub risk: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub is_dir: bool,
    pub report_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub findings: Vec<Finding>,
    pub total_bytes: u64,
    pub rules_matched: usize,
    pub rules_scanned: usize,
    pub skipped_roots: Vec<String>,
}

pub fn scan(pack: &RulePack, opts: &ScanOptions) -> Result<ScanReport> {
    let rules = pack.active_rules(&opts.categories, opts.include_disabled);
    let rules_scanned = rules.len();
    let mut findings = Vec::new();
    let mut skipped_roots = Vec::new();
    let mut matched_rules = std::collections::BTreeSet::new();

    for rule in rules {
        let min_age = opts.min_age_override.unwrap_or(rule.min_age_days);
        let include = build_globset(&rule.include_globs, true)?;
        let exclude = build_globset(&rule.exclude_globs, false)?;

        for template in &rule.paths {
            let Some(root) = expand_path_template(template) else {
                skipped_roots.push(format!("{template} (unexpanded)"));
                continue;
            };
            if !root.exists() {
                continue;
            }
            if !is_safe_rule_root(&root) {
                skipped_roots.push(format!("{} (blocked by safety)", root.display()));
                continue;
            }

            match rule.delete_mode {
                DeleteMode::Path => {
                    if let Some(f) = consider_path(&root, &root, rule, min_age, &include, &exclude)
                    {
                        matched_rules.insert(rule.id.clone());
                        findings.push(f);
                    }
                }
                DeleteMode::Contents => {
                    collect_under(
                        &root,
                        rule,
                        min_age,
                        &include,
                        &exclude,
                        &mut findings,
                        &mut matched_rules,
                    )?;
                }
            }
        }
    }

    // De-dupe by path (keep first / larger rule specificity: first wins)
    findings.sort_by(|a, b| a.path.cmp(&b.path));
    findings.dedup_by(|a, b| a.path == b.path);

    // Prefer not listing parents when children are also listed? Keep both for honesty of sizes;
    // clean() will delete deepest-first.
    let total_bytes = findings.iter().map(|f| f.bytes).sum();
    Ok(ScanReport {
        findings,
        total_bytes,
        rules_matched: matched_rules.len(),
        rules_scanned,
        skipped_roots,
    })
}

fn collect_under(
    root: &Path,
    rule: &Rule,
    min_age: u64,
    include: &GlobSet,
    exclude: &GlobSet,
    findings: &mut Vec<Finding>,
    matched_rules: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    // Prefer top-level cleanable units under the rule root (CleanMyMac/BleachBit style)
    // so sizes are not double-counted across parents and children.
    // If include_globs are set, walk deeper and emit matching files only.
    let deep = !rule.include_globs.is_empty();
    let max_depth = if deep {
        rule.max_depth
    } else {
        1
    };

    let walker = WalkDir::new(root)
        .follow_links(false)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !is_never_touch(e.path()));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if !stays_under_root(root, path) {
            continue;
        }
        if is_never_touch(path) {
            continue;
        }
        if deep && entry.file_type().is_dir() {
            // With include globs, only match files to avoid double-counting.
            continue;
        }
        if let Some(f) = consider_path(path, root, rule, min_age, include, exclude) {
            matched_rules.insert(rule.id.clone());
            findings.push(f);
        }
    }
    Ok(())
}

fn consider_path(
    path: &Path,
    root: &Path,
    rule: &Rule,
    min_age: u64,
    include: &GlobSet,
    exclude: &GlobSet,
) -> Option<Finding> {
    if is_never_touch(path) || !stays_under_root(root, path) {
        return None;
    }
    if !is_old_enough(path, min_age) {
        return None;
    }

    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_str = rel.to_string_lossy();
    // Also test with full path for absolute globs
    let full_str = path.to_string_lossy();

    if exclude.is_match(rel_str.as_ref()) || exclude.is_match(full_str.as_ref()) {
        return None;
    }
    if !include.is_empty()
        && !include.is_match(rel_str.as_ref())
        && !include.is_match(full_str.as_ref())
    {
        return None;
    }

    let meta = path.symlink_metadata().ok()?;
    // Do not follow symlinks for deletion candidates
    if meta.file_type().is_symlink() {
        return None;
    }

    let is_dir = meta.is_dir();
    let bytes = if is_dir {
        dir_size(path)
    } else {
        meta.len()
    };

    // Skip empty noise
    if bytes == 0 && !is_dir {
        return None;
    }

    let report_only = rule.report_only || matches!(rule.risk, Risk::ReportOnly);

    Some(Finding {
        rule_id: rule.id.clone(),
        category: rule.category.clone(),
        risk: rule.risk.as_str().to_string(),
        path: path.to_path_buf(),
        bytes,
        is_dir,
        report_only,
    })
}

fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn build_globset(patterns: &[String], default_all_if_empty: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    if patterns.is_empty() && default_all_if_empty {
        // empty include = match all (handled by is_empty check in consider_path)
        return Ok(builder.build()?);
    }
    for p in patterns {
        builder.add(Glob::new(p)?);
    }
    Ok(builder.build()?)
}
