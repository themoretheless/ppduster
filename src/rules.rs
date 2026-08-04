use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Risk level for a cleaning rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    Low,
    Medium,
    High,
    ReportOnly,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
            Risk::ReportOnly => "report-only",
        }
    }
}

/// Platform filter for a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Any,
    Macos,
    Linux,
    Windows,
}

impl Platform {
    pub fn matches_host(self) -> bool {
        match self {
            Platform::Any => true,
            Platform::Macos => cfg!(target_os = "macos"),
            Platform::Linux => cfg!(target_os = "linux"),
            Platform::Windows => cfg!(target_os = "windows"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Any => "any",
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Windows => "windows",
        }
    }
}

/// How to treat matched paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteMode {
    /// Delete files/dirs under the path, keep the root directory.
    #[default]
    Contents,
    /// Delete the matched path itself.
    Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub platform: Platform,
    #[serde(default = "default_risk")]
    pub risk: Risk,
    #[serde(default = "default_true")]
    pub default_enabled: bool,
    /// Skip files newer than this many days (0 = no age filter).
    #[serde(default = "default_min_age")]
    pub min_age_days: u64,
    /// Path templates with $HOME, $TMPDIR, $XDG_CACHE_HOME, %LOCALAPPDATA%, etc.
    pub paths: Vec<String>,
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    #[serde(default)]
    pub delete_mode: DeleteMode,
    /// If true, only list; never delete even with --yes (unless --all and future override).
    #[serde(default)]
    pub report_only: bool,
}

fn default_category() -> String {
    "misc".into()
}
fn default_risk() -> Risk {
    Risk::Low
}
fn default_true() -> bool {
    true
}
fn default_min_age() -> u64 {
    3
}
fn default_max_depth() -> usize {
    12
}

impl Default for Platform {
    fn default() -> Self {
        Platform::Any
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuleFile {
    #[serde(default)]
    pub pack: Option<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct RulePack {
    pub rules: Vec<Rule>,
    pub sources: Vec<PathBuf>,
}

impl RulePack {
    pub fn load_many(dirs: &[PathBuf]) -> Result<Self> {
        let mut seen_dirs = Vec::new();
        let mut by_id: BTreeMap<String, Rule> = BTreeMap::new();
        let mut sources = Vec::new();

        for dir in dirs {
            let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if seen_dirs.iter().any(|d| d == &canon) {
                continue;
            }
            seen_dirs.push(canon.clone());
            if !dir.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(dir)
                .with_context(|| format!("read rules dir {}", dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x == "yaml" || x == "yml")
                        .unwrap_or(false)
                })
                .collect();
            files.sort();
            for file in files {
                let text = fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                let parsed: RuleFile = serde_yaml::from_str(&text)
                    .with_context(|| format!("parse {}", file.display()))?;
                sources.push(file);
                for rule in parsed.rules {
                    by_id.insert(rule.id.clone(), rule);
                }
            }
        }

        let rules: Vec<Rule> = by_id.into_values().collect();
        if rules.is_empty() {
            anyhow::bail!("no rules loaded from {}", path_list(dirs));
        }
        Ok(RulePack { rules, sources })
    }

    pub fn active_rules(&self, categories: &[String], include_disabled: bool) -> Vec<&Rule> {
        self.rules
            .iter()
            .filter(|r| r.platform.matches_host())
            .filter(|r| include_disabled || r.default_enabled)
            .filter(|r| {
                if categories.is_empty() {
                    true
                } else {
                    categories.iter().any(|c| c == &r.category || c == &r.id)
                }
            })
            .collect()
    }

    pub fn categories(&self, include_disabled: bool) -> BTreeMap<String, usize> {
        let mut map = BTreeMap::new();
        for r in &self.rules {
            if !r.platform.matches_host() {
                continue;
            }
            if !include_disabled && !r.default_enabled {
                continue;
            }
            *map.entry(r.category.clone()).or_insert(0) += 1;
        }
        map
    }

    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }
}

fn path_list(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Expand path templates to concrete absolute paths.
pub fn expand_path_template(template: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let mut s = template.to_string();

    let replacements: Vec<(&str, PathBuf)> = {
        let mut v = vec![
            ("$HOME", home.clone()),
            ("~", home.clone()),
            (
                "$TMPDIR",
                std::env::temp_dir(),
            ),
            (
                "$TEMP",
                std::env::temp_dir(),
            ),
            (
                "$TMP",
                std::env::temp_dir(),
            ),
        ];
        if let Some(cache) = dirs::cache_dir() {
            v.push(("$XDG_CACHE_HOME", cache.clone()));
            v.push(("$CACHE", cache));
        }
        if let Some(data) = dirs::data_local_dir() {
            v.push(("$XDG_DATA_HOME", data.clone()));
            v.push(("%LOCALAPPDATA%", data.clone()));
            v.push(("$LOCALAPPDATA", data));
        }
        if let Some(data) = dirs::data_dir() {
            v.push(("$XDG_CONFIG_HOME", dirs::config_dir().unwrap_or(data.clone())));
            v.push(("%APPDATA%", data.clone()));
            v.push(("$APPDATA", data));
        }
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            v.push(("%USERPROFILE%", PathBuf::from(userprofile)));
        }
        if let Ok(windir) = std::env::var("WINDIR") {
            v.push(("%WINDIR%", PathBuf::from(windir)));
        }
        if let Ok(systemdrive) = std::env::var("SystemDrive") {
            v.push(("%SystemDrive%", PathBuf::from(systemdrive)));
        }
        v
    };

    // Longest keys first so %LOCALAPPDATA% beats partials
    let mut keys = replacements;
    keys.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (key, path) in &keys {
        if s.contains(key) {
            s = s.replace(key, &path.to_string_lossy());
        }
    }

    // Reject unexpanded variables
    if s.contains('$') || s.contains('%') {
        return None;
    }

    let p = PathBuf::from(s);
    if p.exists() {
        Some(p)
    } else {
        // Still return path so scan can report missing roots quietly
        Some(p)
    }
}

pub fn host_platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let p = expand_path_template("$HOME/Library/Caches").unwrap();
        assert!(p.to_string_lossy().contains("Library"));
    }
}
