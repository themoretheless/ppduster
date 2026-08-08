use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    pub timestamp: String,
    pub action: String,
    pub outcome: String,
    pub detail: Option<String>,
}

pub fn resolve_log_path(explicit: Option<&PathBuf>) -> Option<PathBuf> {
    explicit
        .cloned()
        .or_else(|| std::env::var_os("PPDUSTER_AUDIT_LOG").map(PathBuf::from))
        .or_else(|| dirs::data_local_dir().map(|dir| dir.join("ppduster").join("audit.log")))
}

pub fn append_event(
    log_path: &Path,
    action: &str,
    outcome: &str,
    detail: Option<&str>,
) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create audit log directory {}", parent.display()))?;
        }
    }

    let entry = AuditEntry {
        timestamp: Utc::now().to_rfc3339(),
        action: action.to_string(),
        outcome: outcome.to_string(),
        detail: detail.map(str::to_string),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("open audit log {}", log_path.display()))?;
    let line = serde_json::to_string(&entry)?;
    writeln!(file, "{line}")
        .with_context(|| format!("write audit entry to {}", log_path.display()))?;
    Ok(())
}

pub fn read_events(log_path: &Path) -> Result<Vec<AuditEntry>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(log_path)
        .with_context(|| format!("read audit log {}", log_path.display()))?;
    let mut entries = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str(line)?);
    }
    Ok(entries)
}
