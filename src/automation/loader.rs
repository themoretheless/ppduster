use crate::automation::task::{Task, TaskFile, TrustRequirement};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackTrust {
    Bundled,
    UserConfig,
    External,
}

#[derive(Debug, Clone)]
pub struct TaskSource {
    pub path: PathBuf,
    pub trust: PackTrust,
}

#[derive(Debug, Clone)]
pub struct TaskPack {
    pub tasks: Vec<Task>,
    pub sources: Vec<TaskSource>,
}

impl TaskPack {
    pub fn load_many(sources: &[TaskSource], allow_external: bool) -> Result<Self> {
        let mut tasks = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut loaded = Vec::new();
        for source in sources {
            if !source.path.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(&source.path)
                .with_context(|| format!("read tasks dir {}", source.path.display()))?
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
                    .with_context(|| format!("read task file {}", file.display()))?;
                let parsed: TaskFile = serde_yaml::from_str(&text)
                    .with_context(|| format!("parse task file {}", file.display()))?;
                validate_trust(&parsed.task, source.trust, allow_external, &file)?;
                parsed
                    .task
                    .validate()
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("validate task file {}", file.display()))?;
                if !parsed.task.platform.matches_host() {
                    continue;
                }
                if !seen.insert(parsed.task.id.clone()) {
                    anyhow::bail!("duplicate task id {} in {}", parsed.task.id, file.display());
                }
                tasks.push(parsed.task);
                loaded.push(TaskSource {
                    path: file,
                    trust: source.trust,
                });
            }
        }
        Ok(Self {
            tasks,
            sources: loaded,
        })
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

fn validate_trust(task: &Task, trust: PackTrust, allow_external: bool, path: &Path) -> Result<()> {
    match (task.trust, trust) {
        (_, PackTrust::Bundled) => Ok(()),
        (TrustRequirement::BundledOnly, _) => {
            anyhow::bail!(
                "task {} in {} requires bundled trust",
                task.id,
                path.display()
            )
        }
        (TrustRequirement::UserConfigAllowed, PackTrust::External) => anyhow::bail!(
            "task {} in {} requires user-config or bundled trust",
            task.id,
            path.display()
        ),
        (TrustRequirement::ExternalAllowed, PackTrust::External) if !allow_external => {
            anyhow::bail!(
                "external task pack {} blocked; pass --trust-external-packs to allow it",
                path.display()
            )
        }
        _ => Ok(()),
    }
}
