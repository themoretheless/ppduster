use crate::scan::ScanReport;
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct CleanResult {
    pub deleted: Vec<CleanedItem>,
    pub skipped_report_only: usize,
    pub errors: Vec<String>,
    pub freed_bytes: u64,
    pub permanent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanedItem {
    pub path: PathBuf,
    pub bytes: u64,
    pub method: String,
}

pub fn clean(report: &ScanReport, permanent: bool) -> Result<CleanResult> {
    let mut items: Vec<_> = report
        .findings
        .iter()
        .filter(|f| !f.report_only)
        .cloned()
        .collect();

    // Deepest paths first so we remove files before parents
    items.sort_by(|a, b| {
        let da = a.path.components().count();
        let db = b.path.components().count();
        db.cmp(&da).then_with(|| a.path.cmp(&b.path))
    });

    let skipped_report_only = report.findings.iter().filter(|f| f.report_only).count();
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    let mut freed = 0u64;

    for item in items {
        if !item.path.exists() {
            continue;
        }
        // Re-check safety at delete time
        if crate::safety::is_never_touch(&item.path) {
            errors.push(format!("blocked by safety: {}", item.path.display()));
            continue;
        }

        let method = if permanent {
            match permanent_delete(&item.path) {
                Ok(()) => "permanent".to_string(),
                Err(e) => {
                    errors.push(format!("{}: {e}", item.path.display()));
                    continue;
                }
            }
        } else {
            match trash::delete(&item.path) {
                Ok(()) => "trash".to_string(),
                Err(e) => {
                    errors.push(format!("{}: {e}", item.path.display()));
                    continue;
                }
            }
        };

        freed = freed.saturating_add(item.bytes);
        deleted.push(CleanedItem {
            path: item.path,
            bytes: item.bytes,
            method,
        });
    }

    Ok(CleanResult {
        deleted,
        skipped_report_only,
        errors,
        freed_bytes: freed,
        permanent,
    })
}

fn permanent_delete(path: &std::path::Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove_dir_all {}", path.display()))?;
    } else {
        std::fs::remove_file(path).with_context(|| format!("remove_file {}", path.display()))?;
    }
    Ok(())
}
