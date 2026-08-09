use crate::automation::task::{Step, Task, TaskFile, TrustRequirement};
use crate::rules::Platform;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_TEMPLATE_DEPTH: usize = 32;
const MAX_EXPANDED_STEPS: usize = 4096;

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

#[derive(Debug, Clone, Default)]
pub struct TaskPack {
    pub tasks: Vec<Task>,
    pub sources: Vec<TaskSource>,
    origins: BTreeMap<String, PackTrust>,
    unavailable: Vec<(String, Platform)>,
}

impl TaskPack {
    pub fn load_many(sources: &[TaskSource], allow_external: bool) -> Result<Self> {
        Self::load_sources(sources, allow_external, false)
    }

    /// Load task sources in order, allowing a later source to replace a task
    /// with the same id. This is intended for an explicitly selected user file
    /// layered over the bundled library; the normal pack loader remains strict.
    pub fn load_many_with_overrides(sources: &[TaskSource], allow_external: bool) -> Result<Self> {
        Self::load_sources(sources, allow_external, true)
    }

    fn load_sources(
        sources: &[TaskSource],
        allow_external: bool,
        allow_overrides: bool,
    ) -> Result<Self> {
        let mut tasks = Vec::new();
        let mut seen = BTreeSet::new();
        let mut loaded = Vec::new();
        let mut origins = BTreeMap::new();
        let mut unavailable = Vec::new();
        for source in sources {
            let mut files: Vec<PathBuf> = if source.path.is_file() {
                vec![source.path.clone()]
            } else if source.path.is_dir() {
                fs::read_dir(&source.path)
                    .with_context(|| format!("read tasks dir {}", source.path.display()))?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| is_yaml_file(p))
                    .collect()
            } else {
                continue;
            };
            files.sort();
            for file in files {
                if !is_yaml_file(&file) {
                    bail!(
                        "task file {} must use a .yaml or .yml extension",
                        file.display()
                    );
                }
                let text = fs::read_to_string(&file)
                    .with_context(|| format!("read task file {}", file.display()))?;
                reject_license_key_fields(&text, &file)?;
                let parsed: TaskFile = serde_yaml::from_str(&text)
                    .with_context(|| format!("parse task file {}", file.display()))?;
                validate_trust(&parsed.task, source.trust, allow_external, &file)?;
                parsed
                    .task
                    .validate()
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("validate task file {}", file.display()))?;
                if !parsed.task.platform.matches_host() {
                    unavailable.push((parsed.task.id, parsed.task.platform));
                    continue;
                }
                if !seen.insert(parsed.task.id.clone()) {
                    if !allow_overrides {
                        anyhow::bail!("duplicate task id {} in {}", parsed.task.id, file.display());
                    }
                    let index = tasks
                        .iter()
                        .position(|task: &Task| task.id == parsed.task.id)
                        .expect("seen task id must have a corresponding task");
                    tasks.remove(index);
                    loaded.remove(index);
                }
                origins.insert(parsed.task.id.clone(), source.trust);
                tasks.push(parsed.task);
                loaded.push(TaskSource {
                    path: file,
                    trust: source.trust,
                });
            }
        }
        let pack = Self {
            tasks,
            sources: loaded,
            origins,
            unavailable,
        };

        // Resolve every template while loading so broken references, cycles,
        // trust downgrades, and expansion limits fail before a task can be run.
        for task in &pack.tasks {
            pack.resolve(&task.id)
                .with_context(|| format!("resolve task template {}", task.id))?;
        }

        Ok(pack)
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Resolve a scenario or reusable template into one flat task.
    ///
    /// The returned task keeps the root composition as runtime provenance
    /// available through `Task::included_scenarios`, while `steps` contains all
    /// primitive child steps in declaration order. Child step IDs are prefixed
    /// with their inclusion path (for example, `developer-tools/install-ripgrep`)
    /// so reports retain provenance and IDs remain unambiguous across scenario
    /// boundaries.
    pub fn resolve(&self, id: &str) -> Result<Task> {
        let index = self
            .tasks
            .iter()
            .position(|task| task.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown task id {}", id))?;
        if !self.origins.contains_key(id) {
            bail!("scenario {} has no trusted source provenance", id);
        }
        let by_id = self
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| (task.id.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        let mut stack = Vec::new();
        let mut cache = BTreeMap::new();
        let mut seen_scenarios = BTreeSet::from([self.tasks[index].id.clone()]);
        let steps =
            self.resolve_steps(index, &by_id, &mut stack, &mut cache, &mut seen_scenarios)?;
        let mut resolved = self.tasks[index].clone();
        resolved.resolved_scenarios = std::mem::take(&mut resolved.scenarios);
        resolved.steps = steps;
        resolved
            .validate_executable()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("validate resolved scenario {}", resolved.id))?;
        Ok(resolved)
    }

    fn resolve_steps(
        &self,
        index: usize,
        by_id: &BTreeMap<&str, usize>,
        stack: &mut Vec<usize>,
        cache: &mut BTreeMap<usize, Vec<Step>>,
        seen_scenarios: &mut BTreeSet<String>,
    ) -> Result<Vec<Step>> {
        self.tasks[index]
            .validate()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("validate scenario {}", self.tasks[index].id))?;
        if let Some(steps) = cache.get(&index) {
            return Ok(steps.clone());
        }
        if let Some(cycle_start) = stack.iter().position(|entry| *entry == index) {
            let mut cycle = stack[cycle_start..]
                .iter()
                .map(|entry| self.tasks[*entry].id.as_str())
                .collect::<Vec<_>>();
            cycle.push(self.tasks[index].id.as_str());
            bail!("scenario template cycle detected: {}", cycle.join(" -> "));
        }
        if stack.len() >= MAX_TEMPLATE_DEPTH {
            let mut chain = stack
                .iter()
                .map(|entry| self.tasks[*entry].id.as_str())
                .collect::<Vec<_>>();
            chain.push(self.tasks[index].id.as_str());
            bail!(
                "scenario template depth exceeds {}: {}",
                MAX_TEMPLATE_DEPTH,
                chain.join(" -> ")
            );
        }

        stack.push(index);
        let task = &self.tasks[index];
        let mut resolved = if task.scenarios.is_empty() {
            task.steps.clone()
        } else {
            Vec::new()
        };

        for scenario_id in &task.scenarios {
            let Some(child_index) = by_id.get(scenario_id.as_str()).copied() else {
                let mut chain = stack
                    .iter()
                    .map(|entry| self.tasks[*entry].id.as_str())
                    .collect::<Vec<_>>();
                chain.push(scenario_id.as_str());
                if let Some((_, platform)) = self
                    .unavailable
                    .iter()
                    .find(|(unavailable_id, _)| unavailable_id == scenario_id)
                {
                    bail!(
                        "scenario {} referenced by {} is unavailable on this host (platform {}; chain: {})",
                        scenario_id,
                        task.id,
                        platform.as_str(),
                        chain.join(" -> ")
                    );
                }
                bail!(
                    "unknown scenario {} referenced by {} (chain: {})",
                    scenario_id,
                    task.id,
                    chain.join(" -> ")
                );
            };

            let parent_trust = self.origins.get(&task.id).copied().ok_or_else(|| {
                anyhow::anyhow!("scenario {} has no trusted source provenance", task.id)
            })?;
            let child_trust = self.origins.get(scenario_id).copied().ok_or_else(|| {
                anyhow::anyhow!("scenario {} has no trusted source provenance", scenario_id)
            })?;
            if trust_rank(child_trust) < trust_rank(parent_trust) {
                bail!(
                    "scenario {} from {:?} trust cannot include less-trusted scenario {} from {:?} trust",
                    task.id,
                    parent_trust,
                    scenario_id,
                    child_trust
                );
            }

            if !stack.contains(&child_index) && !seen_scenarios.insert(scenario_id.clone()) {
                let mut chain = stack
                    .iter()
                    .map(|entry| self.tasks[*entry].id.as_str())
                    .collect::<Vec<_>>();
                chain.push(scenario_id.as_str());
                bail!(
                    "scenario template {} includes scenario {} more than once (again through chain: {})",
                    self.tasks[*stack.first().unwrap_or(&index)].id,
                    scenario_id,
                    chain.join(" -> ")
                );
            }

            let child_steps =
                self.resolve_steps(child_index, by_id, stack, cache, seen_scenarios)?;
            if resolved.len().saturating_add(child_steps.len()) > MAX_EXPANDED_STEPS {
                bail!(
                    "scenario template {} expands beyond {} steps",
                    task.id,
                    MAX_EXPANDED_STEPS
                );
            }
            resolved.extend(child_steps.into_iter().map(|mut step| {
                step.prefix_condition_step(scenario_id);
                step.id = format!("{}/{}", scenario_id, step.id);
                step
            }));
        }
        stack.pop();

        let mut seen_steps = BTreeSet::new();
        for step in &resolved {
            if !seen_steps.insert(&step.id) {
                bail!(
                    "scenario template {} produces duplicate step id {}",
                    task.id,
                    step.id
                );
            }
        }
        cache.insert(index, resolved.clone());
        Ok(resolved)
    }
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "yaml" || extension == "yml")
}

fn trust_rank(trust: PackTrust) -> u8 {
    match trust {
        PackTrust::External => 0,
        PackTrust::UserConfig => 1,
        PackTrust::Bundled => 2,
    }
}

fn reject_license_key_fields(text: &str, path: &Path) -> Result<()> {
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .with_context(|| format!("parse task file {}", path.display()))?;
    if let Some(field) = find_license_key_field(&value) {
        bail!(
            "task file {} contains forbidden field {}; enter license keys only in the vendor UI",
            path.display(),
            field
        );
    }
    Ok(())
}

fn find_license_key_field(value: &serde_yaml::Value) -> Option<&str> {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if let Some(key) = key.as_str() {
                    let normalized = key
                        .chars()
                        .filter(|ch| ch.is_ascii_alphanumeric())
                        .map(|ch| ch.to_ascii_lowercase())
                        .collect::<String>();
                    if normalized == "licensekey" {
                        return Some(key);
                    }
                }
                if let Some(field) = find_license_key_field(value) {
                    return Some(field);
                }
            }
            None
        }
        serde_yaml::Value::Sequence(values) => values.iter().find_map(find_license_key_field),
        serde_yaml::Value::Tagged(tagged) => find_license_key_field(&tagged.value),
        _ => None,
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
