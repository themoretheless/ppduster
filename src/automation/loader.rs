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
    /// Original v1 steps retained only while resolving legacy `scenarios`
    /// templates. Public tasks and resolved tasks are graph-only v3.
    legacy_steps: BTreeMap<String, Vec<Step>>,
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
        let mut legacy_steps = BTreeMap::new();
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
                let imported_steps = legacy_steps_from_yaml(&text)
                    .with_context(|| format!("parse legacy task steps in {}", file.display()))?;
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
                    legacy_steps.remove(&parsed.task.id);
                }
                origins.insert(parsed.task.id.clone(), source.trust);
                if !imported_steps.is_empty() {
                    legacy_steps.insert(parsed.task.id.clone(), imported_steps);
                }
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
            legacy_steps,
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

    /// Resolve a scenario or reusable template into one executable task.
    ///
    /// The returned task keeps the root composition as runtime provenance
    /// available through `Task::included_scenarios`, while legacy templates are
    /// flattened into `steps` in declaration order. A direct v2 graph is kept
    /// intact. Graph tasks cannot yet be nested in a legacy scenario template:
    /// silently flattening or inferring edges would change their semantics.
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
        if !steps.is_empty() {
            resolved.graph = None;
            resolved.steps = steps;
        }
        resolved
            .into_v3()
            .map_err(anyhow::Error::new)
            .with_context(|| format!("migrate resolved scenario {} to workflow graph v3", id))
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
        let imported_steps = self
            .legacy_steps
            .get(&self.tasks[index].id)
            .cloned()
            .unwrap_or_else(|| self.tasks[index].steps.clone());
        if self.tasks[index].graph.is_some() && imported_steps.is_empty() {
            if let Some(parent_index) = stack.last() {
                bail!(
                    "scenario {} cannot include graph task {}; graph composition must be explicit",
                    self.tasks[*parent_index].id,
                    self.tasks[index].id
                );
            }
            cache.insert(index, Vec::new());
            return Ok(Vec::new());
        }
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
            imported_steps
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

#[derive(serde::Deserialize)]
struct LegacyStepsTaskFile {
    task: LegacyStepsTask,
}

#[derive(serde::Deserialize)]
struct LegacyStepsTask {
    #[serde(default)]
    steps: Vec<Step>,
}

fn legacy_steps_from_yaml(text: &str) -> Result<Vec<Step>> {
    Ok(serde_yaml::from_str::<LegacyStepsTaskFile>(text)?
        .task
        .steps)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::context::{Binding, ContextScope, FieldRef};
    use crate::automation::graph::LegacyTaskImporter;
    use crate::automation::task::{Action, AuthPolicy, ElevationPolicy};

    fn step(id: &str) -> Step {
        Step {
            id: id.into(),
            name: String::new(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::GithubListRepositories,
        }
    }

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            name: id.into(),
            description: format!("Task {id}."),
            platform: Platform::Any,
            trust: TrustRequirement::BundledOnly,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![step("run")],
            graph: None,
        }
    }

    fn pack(tasks: Vec<Task>) -> TaskPack {
        let origins = tasks
            .iter()
            .map(|task| (task.id.clone(), PackTrust::Bundled))
            .collect();
        TaskPack {
            tasks,
            sources: Vec::new(),
            origins,
            unavailable: Vec::new(),
            legacy_steps: BTreeMap::new(),
        }
    }

    #[test]
    fn resolve_preserves_a_direct_v2_graph() {
        let mut graph_task = task("graph-task");
        graph_task.graph = Some(LegacyTaskImporter::import_steps(&graph_task.steps).unwrap());
        graph_task.steps.clear();

        let resolved = pack(vec![graph_task]).resolve("graph-task").unwrap();
        assert!(resolved.steps.is_empty());
        assert!(resolved.graph.is_some());
        resolved.validate_executable().unwrap();
    }

    #[test]
    fn legacy_scenario_template_does_not_silently_flatten_graph_child() {
        let mut child = task("graph-child");
        child.graph = Some(LegacyTaskImporter::import_steps(&child.steps).unwrap());
        child.steps.clear();

        let mut parent = task("parent");
        parent.steps.clear();
        parent.scenarios = vec!["graph-child".into()];

        let error = pack(vec![parent, child]).resolve("parent").unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot include graph task graph-child"));
    }

    #[test]
    fn scenario_expansion_prefixes_linear_binding_sources() {
        let mut child = task("child");
        child.steps[0].id = "list".into();
        let mut inspect = step("inspect");
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(
                FieldRef::step("list")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("https_url"),
            ),
        );
        child.steps.push(inspect);

        let mut parent = task("parent");
        parent.steps.clear();
        parent.scenarios = vec!["child".into()];

        let resolved = pack(vec![parent, child]).resolve("parent").unwrap();
        assert!(resolved.steps.is_empty());
        let graph = resolved.workflow_graph().unwrap();
        assert_eq!(graph.nodes[0].id(), "child/list");
        assert_eq!(graph.nodes[1].id(), "child/inspect");
        let crate::automation::graph::GraphNode::Action(inspect) = &graph.nodes[1] else {
            panic!("expected imported inspect action")
        };
        let Binding::Field { field } = &inspect.bindings["repo"] else {
            panic!("expected field binding")
        };
        assert_eq!(
            field.scope,
            ContextScope::Step {
                step_id: "child/list".into()
            }
        );
        assert_eq!(graph.version, crate::automation::WORKFLOW_GRAPH_VERSION);
        resolved.validate_executable().unwrap();
    }
}
