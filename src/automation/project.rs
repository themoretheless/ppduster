//! Persistent project model shared by the scenario composer and other clients.

use super::{Task, TaskFile, TrustRequirement};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Top-level YAML envelope for a ppduster scenario project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioProjectFile {
    pub project: ScenarioProject,
}

/// A collection of scenario tasks, their grouping, and composer layout state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioProject {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub entries: Vec<ProjectEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub canvases: BTreeMap<String, ComposerCanvas>,
}

/// A persisted node position in the scenario composer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CanvasPoint {
    pub x: f32,
    pub y: f32,
}

/// Persisted visual layout for one scenario task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposerCanvas {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub positions: BTreeMap<String, CanvasPoint>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parents: BTreeMap<String, String>,
}

/// An entry in a project's nested navigation tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProjectEntry {
    Group {
        id: String,
        name: String,
        #[serde(default)]
        entries: Vec<ProjectEntry>,
    },
    Scenario {
        task: Box<Task>,
    },
}

impl ScenarioProject {
    /// Returns the scenario task at an entry-index path.
    pub fn scenario(&self, path: &[usize]) -> Option<&Task> {
        project_entry(&self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task.as_ref()),
            ProjectEntry::Group { .. } => None,
        })
    }

    /// Returns the mutable scenario task at an entry-index path.
    pub fn scenario_mut(&mut self, path: &[usize]) -> Option<&mut Task> {
        project_entry_mut(&mut self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task.as_mut()),
            ProjectEntry::Group { .. } => None,
        })
    }
}

/// Returns an entry at an index path through nested groups.
pub fn project_entry<'a>(entries: &'a [ProjectEntry], path: &[usize]) -> Option<&'a ProjectEntry> {
    let (index, rest) = path.split_first()?;
    let entry = entries.get(*index)?;
    if rest.is_empty() {
        return Some(entry);
    }
    match entry {
        ProjectEntry::Group { entries, .. } => project_entry(entries, rest),
        ProjectEntry::Scenario { .. } => None,
    }
}

/// Returns a mutable entry at an index path through nested groups.
pub fn project_entry_mut<'a>(
    entries: &'a mut [ProjectEntry],
    path: &[usize],
) -> Option<&'a mut ProjectEntry> {
    let (index, rest) = path.split_first()?;
    let entry = entries.get_mut(*index)?;
    if rest.is_empty() {
        return Some(entry);
    }
    match entry {
        ProjectEntry::Group { entries, .. } => project_entry_mut(entries, rest),
        ProjectEntry::Scenario { .. } => None,
    }
}

/// Returns the entries owned by the group at `path`, or root entries for an empty path.
pub fn project_group_entries<'a>(
    project: &'a ScenarioProject,
    path: &[usize],
) -> Option<&'a [ProjectEntry]> {
    if path.is_empty() {
        return Some(&project.entries);
    }
    match project_entry(&project.entries, path)? {
        ProjectEntry::Group { entries, .. } => Some(entries),
        ProjectEntry::Scenario { .. } => None,
    }
}

/// Returns mutable entries owned by the group at `path`, or root entries for an empty path.
pub fn project_group_entries_mut<'a>(
    project: &'a mut ScenarioProject,
    path: &[usize],
) -> Option<&'a mut Vec<ProjectEntry>> {
    if path.is_empty() {
        return Some(&mut project.entries);
    }
    match project_entry_mut(&mut project.entries, path)? {
        ProjectEntry::Group { entries, .. } => Some(entries),
        ProjectEntry::Scenario { .. } => None,
    }
}

/// Validates project metadata, every scenario task, and scenario ID uniqueness.
pub fn validate_project(project: &ScenarioProject) -> Result<(), String> {
    if project.id.trim().is_empty() {
        return Err("project id must not be empty".into());
    }
    if project.name.trim().is_empty() {
        return Err(format!("project {} name must not be empty", project.id));
    }
    let mut ids = BTreeSet::new();
    validate_project_entries(&project.entries, &mut ids)
}

fn validate_project_entries(
    entries: &[ProjectEntry],
    scenario_ids: &mut BTreeSet<String>,
) -> Result<(), String> {
    for entry in entries {
        match entry {
            ProjectEntry::Group { id, name, entries } => {
                if id.trim().is_empty() || name.trim().is_empty() {
                    return Err("project groups require id and name".into());
                }
                validate_project_entries(entries, scenario_ids)?;
            }
            ProjectEntry::Scenario { task } => {
                task.validate()?;
                if !scenario_ids.insert(task.id.clone()) {
                    return Err(format!(
                        "project contains duplicate scenario id {}",
                        task.id
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Loads a project YAML document, wrapping a legacy standalone task when needed.
pub fn load_project_yaml(yaml: &str) -> anyhow::Result<ScenarioProject> {
    if let Ok(file) = serde_yaml::from_str::<ScenarioProjectFile>(yaml) {
        return Ok(file.project);
    }
    let task = serde_yaml::from_str::<TaskFile>(yaml)
        .context("файл не является проектом или сценарием ppduster")?
        .task;
    let id = format!("{}-project", task.id);
    let name = format!("Проект: {}", task.name);
    Ok(ScenarioProject {
        id,
        name,
        description: "Импортирован из одиночного сценария ppduster.".into(),
        canvases: BTreeMap::new(),
        entries: vec![ProjectEntry::Group {
            id: "imported".into(),
            name: "Импортированные сценарии".into(),
            entries: vec![ProjectEntry::Scenario {
                task: Box::new(task),
            }],
        }],
    })
}

/// Marks every project scenario as external and drops resolved runtime provenance.
pub fn make_project_external(entries: &mut [ProjectEntry]) {
    for entry in entries {
        match entry {
            ProjectEntry::Group { entries, .. } => make_project_external(entries),
            ProjectEntry::Scenario { task } => {
                task.trust = TrustRequirement::ExternalAllowed;
                task.resolved_scenarios.clear();
            }
        }
    }
}

/// Finds the first scenario in depth-first entry order.
pub fn first_scenario_path(
    entries: &[ProjectEntry],
    prefix: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    for (index, entry) in entries.iter().enumerate() {
        prefix.push(index);
        match entry {
            ProjectEntry::Scenario { .. } => return Some(prefix.clone()),
            ProjectEntry::Group { entries, .. } => {
                if let Some(path) = first_scenario_path(entries, prefix) {
                    return Some(path);
                }
            }
        }
        prefix.pop();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Platform;

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            name: format!("Task {id}"),
            description: format!("Scenario fixture for {id}."),
            platform: Platform::Macos,
            trust: TrustRequirement::BundledOnly,
            scenarios: vec!["shared-scenario".into()],
            resolved_scenarios: vec!["runtime-source".into()],
            graph: None,
            steps: Vec::new(),
        }
    }

    fn nested_project() -> ScenarioProject {
        ScenarioProject {
            id: "workstation".into(),
            name: "Workstation".into(),
            description: "Developer workstation project.".into(),
            canvases: BTreeMap::new(),
            entries: vec![ProjectEntry::Group {
                id: "development".into(),
                name: "Development".into(),
                entries: vec![ProjectEntry::Group {
                    id: "git".into(),
                    name: "Git".into(),
                    entries: vec![ProjectEntry::Scenario {
                        task: Box::new(task("nested-scenario")),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn project_round_trips_nested_entries_and_canvas_layout() {
        let mut project = nested_project();
        project.canvases.insert(
            "nested-scenario".into(),
            ComposerCanvas {
                positions: BTreeMap::from([
                    ("start".into(), CanvasPoint { x: 80.0, y: 250.0 }),
                    ("inspect".into(), CanvasPoint { x: 366.0, y: 170.0 }),
                ]),
                parents: BTreeMap::from([("inspect".into(), "start".into())]),
            },
        );

        validate_project(&project).unwrap();
        let yaml = serde_yaml::to_string(&ScenarioProjectFile { project }).unwrap();
        let reparsed = load_project_yaml(&yaml).unwrap();
        let path = first_scenario_path(&reparsed.entries, &mut Vec::new()).unwrap();

        assert_eq!(path, vec![0, 0, 0]);
        assert_eq!(reparsed.scenario(&path).unwrap().id, "nested-scenario");
        assert_eq!(
            reparsed.canvases["nested-scenario"].positions["inspect"].y,
            170.0
        );
        assert_eq!(
            reparsed.canvases["nested-scenario"].parents["inspect"],
            "start"
        );
    }

    #[test]
    fn group_helpers_support_root_nested_and_mutable_paths() {
        let mut project = nested_project();

        assert_eq!(project_group_entries(&project, &[]).unwrap().len(), 1);
        assert!(matches!(
            project_group_entries(&project, &[0]).unwrap().first(),
            Some(ProjectEntry::Group { id, .. }) if id == "git"
        ));
        assert!(project_group_entries(&project, &[0, 0, 0]).is_none());

        project_group_entries_mut(&mut project, &[0, 0])
            .unwrap()
            .push(ProjectEntry::Scenario {
                task: Box::new(task("second-scenario")),
            });
        assert_eq!(project.scenario(&[0, 0, 1]).unwrap().id, "second-scenario");
    }

    #[test]
    fn loader_wraps_legacy_task_and_externalizes_runtime_state() {
        let yaml = serde_yaml::to_string(&TaskFile {
            task: task("legacy"),
        })
        .unwrap();
        let mut project = load_project_yaml(&yaml).unwrap();
        let path = first_scenario_path(&project.entries, &mut Vec::new()).unwrap();

        assert_eq!(project.id, "legacy-project");
        assert_eq!(project.scenario(&path).unwrap().id, "legacy");

        make_project_external(&mut project.entries);
        let imported = project.scenario(&path).unwrap();
        assert_eq!(imported.trust, TrustRequirement::ExternalAllowed);
        assert!(imported.resolved_scenarios.is_empty());
    }

    #[test]
    fn validation_rejects_duplicate_scenario_ids() {
        let mut project = nested_project();
        project.entries.push(ProjectEntry::Scenario {
            task: Box::new(task("nested-scenario")),
        });

        assert_eq!(
            validate_project(&project).unwrap_err(),
            "project contains duplicate scenario id nested-scenario"
        );
    }
}
