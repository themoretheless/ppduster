use anyhow::Context;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::PackTrust;
use ppduster::automation::{
    describe_step, run_task, Action, AuthPolicy, CopyPathAction, CreateDirectoryAction,
    InspectPathAction, ReleaseChannel, RemovePathAction, RunOptions, RunReport, ScriptInterpreter,
    Step, StepStatus, Task, TaskFile, TaskPack, TaskSource, TrustRequirement, WriteConflictPolicy,
    WriteFileAction,
};
use ppduster::github::{list_accessible_repositories, login_via_web, GithubRepository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const PAPER: Color32 = Color32::from_rgb(246, 245, 239);
const CARD: Color32 = Color32::from_rgb(255, 254, 250);
const INK: Color32 = Color32::from_rgb(32, 34, 31);
const MUTED: Color32 = Color32::from_rgb(124, 129, 122);
const LINE: Color32 = Color32::from_rgb(222, 223, 216);
const PURPLE: Color32 = Color32::from_rgb(101, 87, 217);
const CYAN: Color32 = Color32::from_rgb(21, 146, 136);
const ORANGE: Color32 = Color32::from_rgb(208, 106, 53);
const BLUE: Color32 = Color32::from_rgb(54, 127, 187);

#[derive(Debug, Clone)]
struct ScenarioGroup {
    id: String,
    name: String,
    description: String,
    step_count: usize,
    step_summaries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScenarioProjectFile {
    project: ScenarioProject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScenarioProject {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    entries: Vec<ProjectEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    canvases: BTreeMap<String, ComposerCanvas>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CanvasPoint {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ComposerCanvas {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    positions: BTreeMap<String, CanvasPoint>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    parents: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ProjectEntry {
    Group {
        id: String,
        name: String,
        #[serde(default)]
        entries: Vec<ProjectEntry>,
    },
    Scenario {
        task: Task,
    },
}

impl ScenarioProject {
    fn scenario(&self, path: &[usize]) -> Option<&Task> {
        project_entry(&self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task),
            ProjectEntry::Group { .. } => None,
        })
    }

    fn scenario_mut(&mut self, path: &[usize]) -> Option<&mut Task> {
        project_entry_mut(&mut self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task),
            ProjectEntry::Group { .. } => None,
        })
    }
}

fn project_entry<'a>(entries: &'a [ProjectEntry], path: &[usize]) -> Option<&'a ProjectEntry> {
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

fn project_entry_mut<'a>(
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

#[derive(Debug, Clone, Copy)]
enum ComposerBlockKind {
    GithubListRepositories,
    GitInspect,
    GitCloneIfMissing,
    GitFetch,
    GitFastForward,
    CreateDirectory,
    InspectPath,
    CopyPath,
    WriteFile,
    RemovePath,
    BrewInstall,
}

impl ComposerBlockKind {
    const ALL: [Self; 11] = [
        Self::GithubListRepositories,
        Self::GitInspect,
        Self::GitCloneIfMissing,
        Self::GitFetch,
        Self::GitFastForward,
        Self::CreateDirectory,
        Self::InspectPath,
        Self::CopyPath,
        Self::WriteFile,
        Self::RemovePath,
        Self::BrewInstall,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::GithubListRepositories => "Получить репозитории аккаунта",
            Self::GitInspect => "Проверить Git-репозиторий",
            Self::GitCloneIfMissing => "Клонировать, если отсутствует",
            Self::GitFetch => "Получить remote-ветку",
            Self::GitFastForward => "Актуализировать ветку",
            Self::CreateDirectory => "Создать папку",
            Self::InspectPath => "Проверить путь",
            Self::CopyPath => "Копировать путь",
            Self::WriteFile => "Записать файл",
            Self::RemovePath => "Переместить в корзину",
            Self::BrewInstall => "Установить Homebrew-пакет",
        }
    }

    fn category(self) -> &'static str {
        match self {
            Self::GithubListRepositories => "GITHUB",
            Self::GitInspect | Self::GitCloneIfMissing | Self::GitFetch | Self::GitFastForward => {
                "GIT"
            }
            Self::CreateDirectory
            | Self::InspectPath
            | Self::CopyPath
            | Self::WriteFile
            | Self::RemovePath => "ФАЙЛЫ",
            Self::BrewInstall => "ПАКЕТЫ",
        }
    }
}

struct GithubPickerState {
    open: bool,
    search: String,
    destination_root: String,
    repositories: Vec<GithubRepository>,
    selected_ids: BTreeSet<String>,
    loaded_once: bool,
    loading: bool,
    authorizing: bool,
    error: Option<String>,
    receiver: Option<Receiver<Result<Vec<GithubRepository>, String>>>,
    auth_receiver: Option<Receiver<Result<(), String>>>,
}

impl Default for GithubPickerState {
    fn default() -> Self {
        Self {
            open: false,
            search: String::new(),
            destination_root: default_github_destination_root(),
            repositories: Vec::new(),
            selected_ids: BTreeSet::new(),
            loaded_once: false,
            loading: false,
            authorizing: false,
            error: None,
            receiver: None,
            auth_receiver: None,
        }
    }
}

fn main() -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("ppduster · Scenario Flow")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([980.0, 680.0]);
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_title_shown(false)
        .with_titlebar_shown(false)
        .with_titlebar_buttons_shown(true);
    eframe::run_native(
        "ppduster · Scenario Flow",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(ScenarioApp::new(cc)))),
    )
}

struct ScenarioApp {
    task_pack: Option<TaskPack>,
    load_error: Option<String>,
    selected_task: usize,
    selected_step: Option<usize>,
    channel: ReleaseChannel,
    allow_shell: bool,
    allow_elevation: bool,
    report: Option<RunReport>,
    report_applied: bool,
    plan_error: Option<String>,
    dark: bool,
    confirm_run: bool,
    running: bool,
    run_receiver: Option<Receiver<Result<RunReport, String>>>,
    github_picker: GithubPickerState,
    file_message: Option<(bool, String)>,
    custom_project: Option<ScenarioProject>,
    selected_project_scenario: Option<Vec<usize>>,
    selected_project_group: Vec<usize>,
    block_picker_parent: Option<String>,
    block_picker_search: String,
}

impl ScenarioApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx, false);
        let (task_pack, load_error) = match load_tasks() {
            Ok(pack) => (Some(pack), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let selected_task = task_pack
            .as_ref()
            .and_then(|pack| {
                pack.tasks
                    .iter()
                    .position(|task| task.id == "macos-developer-workstation")
                    .or_else(|| pack.tasks.iter().position(Task::is_template))
            })
            .unwrap_or(0);
        Self {
            task_pack,
            load_error,
            selected_task,
            selected_step: Some(0),
            channel: ReleaseChannel::Release,
            allow_shell: false,
            allow_elevation: false,
            report: None,
            report_applied: false,
            plan_error: None,
            dark: false,
            confirm_run: false,
            running: false,
            run_receiver: None,
            github_picker: GithubPickerState::default(),
            file_message: None,
            custom_project: None,
            selected_project_scenario: None,
            selected_project_group: Vec::new(),
            block_picker_parent: None,
            block_picker_search: String::new(),
        }
    }

    fn selected_task(&self) -> Option<&Task> {
        if let Some(project) = &self.custom_project {
            return project.scenario(self.selected_project_scenario.as_deref()?);
        }
        self.task_pack.as_ref()?.tasks.get(self.selected_task)
    }

    fn resolved_selected_task(&self) -> anyhow::Result<Task> {
        let task = self
            .selected_task()
            .ok_or_else(|| anyhow::anyhow!("сценарий не выбран"))?;
        if self.custom_project.is_some() {
            if github_picker_source_steps(task).is_none() {
                return Ok(task.clone());
            }
            return materialize_github_repositories(
                task.clone(),
                &self.github_picker.repositories,
                &self.github_picker.selected_ids,
                &self.github_picker.destination_root,
            );
        }
        let resolved = self
            .task_pack
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("библиотека сценариев не загружена"))?
            .resolve(&task.id)?;
        if github_picker_source_steps(&resolved).is_some() {
            materialize_github_repositories(
                resolved,
                &self.github_picker.repositories,
                &self.github_picker.selected_ids,
                &self.github_picker.destination_root,
            )
        } else {
            Ok(resolved)
        }
    }

    fn invalidate_plan(&mut self) {
        self.report = None;
        self.report_applied = false;
        self.plan_error = None;
        self.confirm_run = false;
    }

    fn start_custom_project(&mut self) {
        if self.running {
            return;
        }
        let task = Task {
            id: "custom-scenario".into(),
            name: "Новый сценарий".into(),
            description: "Сценарий, собранный из атомарных операций в ppduster.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: Vec::new(),
        };
        self.custom_project = Some(ScenarioProject {
            id: "scenario-project".into(),
            name: "Новый проект".into(),
            description: "Проект сценариев ppduster.".into(),
            canvases: BTreeMap::new(),
            entries: vec![ProjectEntry::Group {
                id: "main".into(),
                name: "Основные сценарии".into(),
                entries: vec![ProjectEntry::Scenario { task }],
            }],
        });
        self.selected_project_scenario = Some(vec![0, 0]);
        self.selected_project_group = vec![0];
        self.selected_step = None;
        self.github_picker.open = false;
        self.github_picker.selected_ids.clear();
        self.invalidate_plan();
    }

    fn add_composer_block(&mut self, kind: ComposerBlockKind) {
        let parent = self
            .block_picker_parent
            .clone()
            .unwrap_or_else(|| "start".into());
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(self.selected_project_scenario.as_deref()?))
        else {
            return;
        };
        let base_id = composer_block_id(kind);
        let mut suffix = task.steps.len() + 1;
        let id = loop {
            let candidate = format!("{base_id}-{suffix}");
            if task.steps.iter().all(|step| step.id != candidate) {
                break candidate;
            }
            suffix += 1;
        };
        task.steps.push(composer_step(kind, id.clone()));
        let task_id = task.id.clone();
        self.selected_step = Some(task.steps.len() - 1);
        if let Some(project) = self.custom_project.as_mut() {
            let canvas = project.canvases.entry(task_id).or_default();
            canvas.parents.insert(id.clone(), parent.clone());
            let parent_position = canvas
                .positions
                .get(&parent)
                .copied()
                .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
            let sibling_index = canvas
                .parents
                .iter()
                .filter(|(child, candidate)| child.as_str() != id && *candidate == &parent)
                .count();
            let branch = branch_offset(sibling_index);
            canvas.positions.insert(
                id,
                CanvasPoint {
                    x: parent_position.x + 286.0,
                    y: (parent_position.y + branch).max(40.0),
                },
            );
        }
        self.block_picker_parent = None;
        self.block_picker_search.clear();
        self.invalidate_plan();
    }

    fn open_block_picker(&mut self, parent: impl Into<String>) {
        if self.running || self.custom_project.is_none() {
            return;
        }
        self.block_picker_parent = Some(parent.into());
        self.block_picker_search.clear();
    }

    fn ensure_composer_canvas(&mut self, task: &Task) {
        let Some(project) = self.custom_project.as_mut() else {
            return;
        };
        let canvas = project.canvases.entry(task.id.clone()).or_default();
        canvas
            .positions
            .entry("start".into())
            .or_insert(CanvasPoint { x: 80.0, y: 250.0 });

        let valid_ids = task
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<BTreeSet<_>>();
        canvas
            .positions
            .retain(|id, _| id == "start" || valid_ids.contains(id.as_str()));
        canvas.parents.retain(|child, parent| {
            valid_ids.contains(child.as_str())
                && (parent == "start" || valid_ids.contains(parent.as_str()))
        });

        let mut previous = "start".to_owned();
        for step in &task.steps {
            canvas
                .parents
                .entry(step.id.clone())
                .or_insert_with(|| previous.clone());
            if !canvas.positions.contains_key(&step.id) {
                let parent = canvas
                    .parents
                    .get(&step.id)
                    .cloned()
                    .unwrap_or_else(|| previous.clone());
                let parent_position = canvas
                    .positions
                    .get(&parent)
                    .copied()
                    .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
                let sibling_index = canvas
                    .parents
                    .iter()
                    .filter(|(child, candidate)| child.as_str() != step.id && **candidate == parent)
                    .count();
                canvas.positions.insert(
                    step.id.clone(),
                    CanvasPoint {
                        x: parent_position.x + 286.0,
                        y: (parent_position.y + branch_offset(sibling_index)).max(40.0),
                    },
                );
            }
            previous = step.id.clone();
        }
    }

    fn drag_composer_node(&mut self, task_id: &str, node_id: &str, delta: Vec2) {
        if delta == Vec2::ZERO {
            return;
        }
        let Some(position) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.canvases.get_mut(task_id))
            .and_then(|canvas| canvas.positions.get_mut(node_id))
        else {
            return;
        };
        position.x = (position.x + delta.x).max(24.0);
        position.y = (position.y + delta.y).max(210.0);
    }

    fn move_composer_step(&mut self, from: usize, to: usize) {
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(self.selected_project_scenario.as_deref()?))
        else {
            return;
        };
        if from >= task.steps.len() || to >= task.steps.len() || from == to {
            return;
        }
        let step = task.steps.remove(from);
        task.steps.insert(to, step);
        self.selected_step = Some(to);
        self.invalidate_plan();
    }

    fn remove_composer_step(&mut self, index: usize) {
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(self.selected_project_scenario.as_deref()?))
        else {
            return;
        };
        if index >= task.steps.len() {
            return;
        }
        let removed_id = task.steps[index].id.clone();
        let task_id = task.id.clone();
        task.steps.remove(index);
        self.selected_step = if task.steps.is_empty() {
            None
        } else {
            Some(index.min(task.steps.len() - 1))
        };
        if let Some(canvas) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.canvases.get_mut(&task_id))
        {
            let parent = canvas
                .parents
                .remove(&removed_id)
                .unwrap_or_else(|| "start".into());
            canvas.positions.remove(&removed_id);
            for child_parent in canvas.parents.values_mut() {
                if *child_parent == removed_id {
                    *child_parent = parent.clone();
                }
            }
        }
        self.invalidate_plan();
    }

    fn add_project_group(&mut self) {
        let path = self.selected_project_group.clone();
        let Some(project) = self.custom_project.as_mut() else {
            return;
        };
        let Some(entries) = project_group_entries_mut(project, &path) else {
            return;
        };
        let ordinal = entries.len() + 1;
        entries.push(ProjectEntry::Group {
            id: format!("group-{ordinal}"),
            name: format!("Новая группа {ordinal}"),
            entries: Vec::new(),
        });
        let mut new_path = path;
        new_path.push(entries.len() - 1);
        self.selected_project_group = new_path;
        self.invalidate_plan();
    }

    fn add_project_scenario(&mut self) {
        let path = self.selected_project_group.clone();
        let Some(project) = self.custom_project.as_mut() else {
            return;
        };
        let Some(entries) = project_group_entries_mut(project, &path) else {
            return;
        };
        let ordinal = entries.len() + 1;
        entries.push(ProjectEntry::Scenario {
            task: Task {
                id: format!("scenario-{ordinal}"),
                name: format!("Новый сценарий {ordinal}"),
                description: "Сценарий, собранный из атомарных операций в ppduster.".into(),
                platform: ppduster::rules::Platform::Macos,
                trust: TrustRequirement::ExternalAllowed,
                scenarios: Vec::new(),
                resolved_scenarios: Vec::new(),
                steps: Vec::new(),
            },
        });
        let mut scenario_path = path;
        scenario_path.push(entries.len() - 1);
        self.selected_project_scenario = Some(scenario_path);
        self.selected_step = None;
        self.invalidate_plan();
    }

    fn add_github_project_scenario(&mut self) {
        let path = self.selected_project_group.clone();
        let Some(project) = self.custom_project.as_mut() else {
            return;
        };
        let Some(entries) = project_group_entries_mut(project, &path) else {
            return;
        };
        let ordinal = entries.len() + 1;
        entries.push(ProjectEntry::Scenario {
            task: github_repository_composer_task(ordinal),
        });
        let mut scenario_path = path;
        scenario_path.push(entries.len() - 1);
        self.selected_project_scenario = Some(scenario_path);
        self.selected_step = Some(0);
        self.invalidate_plan();
    }

    fn start_github_repository_load(&mut self, ctx: &egui::Context) {
        if self.github_picker.loading {
            return;
        }
        if !self.github_picker.selected_ids.is_empty() {
            self.invalidate_plan();
        }
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = list_accessible_repositories().map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            repaint.request_repaint();
        });
        self.github_picker.receiver = Some(receiver);
        self.github_picker.loading = true;
        self.github_picker.error = None;
    }

    fn poll_github_repository_load(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.github_picker.receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(repositories)) => {
                let selection_uses_loaded_metadata = !self.github_picker.selected_ids.is_empty();
                self.github_picker.repositories = repositories;
                self.github_picker.loaded_once = true;
                self.github_picker.error = None;
                self.github_picker.loading = false;
                self.github_picker.receiver = None;
                if selection_uses_loaded_metadata {
                    self.invalidate_plan();
                }
            }
            Ok(Err(error)) => {
                self.github_picker.error = Some(error);
                self.github_picker.loading = false;
                self.github_picker.receiver = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_picker.error =
                    Some("Фоновая загрузка репозиториев неожиданно завершилась".into());
                self.github_picker.loading = false;
                self.github_picker.receiver = None;
            }
        }
    }

    fn start_github_authorization(&mut self, ctx: &egui::Context) {
        if self.github_picker.authorizing || self.github_picker.loading {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = login_via_web().map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            repaint.request_repaint();
        });
        self.github_picker.auth_receiver = Some(receiver);
        self.github_picker.authorizing = true;
        self.github_picker.error = None;
    }

    fn poll_github_authorization(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.github_picker.auth_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                self.github_picker.authorizing = false;
                self.github_picker.auth_receiver = None;
                self.start_github_repository_load(ctx);
            }
            Ok(Err(error)) => {
                self.github_picker.error = Some(error);
                self.github_picker.authorizing = false;
                self.github_picker.auth_receiver = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.github_picker.error =
                    Some("Фоновая авторизация GitHub неожиданно завершилась".into());
                self.github_picker.authorizing = false;
                self.github_picker.auth_receiver = None;
            }
        }
    }

    fn build_plan(&mut self) {
        self.report_applied = false;
        let task = match self.resolved_selected_task() {
            Ok(task) => task,
            Err(error) => {
                self.report = None;
                self.plan_error = Some(format!("{error:#}"));
                return;
            }
        };
        match run_task(&task, &self.options_for(&task, false)) {
            Ok(report) => {
                self.report = Some(report);
                self.plan_error = None;
            }
            Err(error) => {
                self.report = None;
                self.plan_error = Some(error.to_string());
            }
        }
    }

    fn options_for(&self, task: &Task, apply: bool) -> RunOptions {
        RunOptions {
            apply,
            allow_shell: self.allow_shell,
            allow_elevation: self.allow_elevation,
            release_channel: task
                .steps
                .iter()
                .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
                .then_some(self.channel),
        }
    }

    fn start_run(&mut self, ctx: &egui::Context) {
        self.report = None;
        self.report_applied = false;
        let task = match self.resolved_selected_task() {
            Ok(task) => task,
            Err(error) => {
                self.plan_error = Some(format!("{error:#}"));
                self.confirm_run = false;
                return;
            }
        };
        let options = self.options_for(&task, true);
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = run_task(&task, &options).map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            repaint.request_repaint();
        });
        self.run_receiver = Some(receiver);
        self.running = true;
        self.confirm_run = false;
        self.plan_error = None;
    }

    fn poll_run(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.run_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(report)) => {
                self.report = Some(report);
                self.report_applied = true;
                self.running = false;
                self.run_receiver = None;
            }
            Ok(Err(error)) => {
                self.plan_error = Some(error);
                self.running = false;
                self.run_receiver = None;
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.plan_error = Some("Фоновый запуск неожиданно завершился".into());
                self.running = false;
                self.run_receiver = None;
            }
        }
    }

    fn command_for_selected(&self) -> Option<String> {
        if self.custom_project.is_some() || !self.github_picker.selected_ids.is_empty() {
            return None;
        }
        let task = self.selected_task()?;
        let resolved = self.resolved_selected_task().ok()?;
        let mut command = format!("ppduster setup run {}", task.id);
        if resolved
            .steps
            .iter()
            .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
        {
            command.push_str(match self.channel {
                ReleaseChannel::Release => " --channel release",
                ReleaseChannel::Beta => " --channel beta",
            });
        }
        if self.allow_shell {
            command.push_str(" --allow-shell");
        }
        if self.allow_elevation {
            command.push_str(" --allow-elevation");
        }
        command.push_str(" --yes");
        Some(command)
    }

    fn save_selected_scenario(&mut self) {
        if let Some(project) = self.custom_project.clone() {
            if let Err(error) = validate_project(&project) {
                self.file_message = Some((true, format!("Проект нельзя сохранить: {error}")));
                return;
            }
            let suggested_name = format!("{}.ppduster.yaml", project.id);
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Проект ppduster", &["yaml", "yml"])
                .set_file_name(&suggested_name)
                .save_file()
            else {
                return;
            };
            let result = serde_yaml::to_string(&ScenarioProjectFile { project })
                .map_err(anyhow::Error::from)
                .and_then(|yaml| {
                    fs::write(&path, yaml)
                        .map_err(anyhow::Error::from)
                        .with_context(|| format!("не удалось сохранить {}", path.display()))
                });
            self.file_message = Some(match result {
                Ok(()) => (false, format!("Проект сохранён: {}", path.display())),
                Err(error) => (true, format!("{error:#}")),
            });
            return;
        }
        let Some(mut task) = self.selected_task().cloned() else {
            return;
        };
        if let Err(error) = task.validate() {
            self.file_message = Some((true, format!("Сценарий нельзя сохранить: {error}")));
            return;
        }
        // A file chosen by the user is external on its next load, even if its
        // source scenario was bundled with the application.
        task.trust = TrustRequirement::ExternalAllowed;
        let suggested_name = format!("{}.yaml", task.id);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Проект или сценарий YAML", &["yaml", "yml"])
            .set_file_name(&suggested_name)
            .save_file()
        else {
            return;
        };
        let result = serde_yaml::to_string(&TaskFile { task })
            .map_err(anyhow::Error::from)
            .and_then(|yaml| {
                fs::write(&path, yaml)
                    .map_err(anyhow::Error::from)
                    .with_context(|| format!("не удалось сохранить {}", path.display()))
            });
        self.file_message = Some(match result {
            Ok(()) => (false, format!("Сценарий сохранён: {}", path.display())),
            Err(error) => (true, format!("{error:#}")),
        });
    }

    fn load_scenario_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Проект YAML", &["yaml", "yml"])
            .pick_file()
        else {
            return;
        };
        let loaded = fs::read_to_string(&path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))
            .and_then(|yaml| load_project_yaml(&yaml))
            .and_then(|project| {
                validate_project(&project).map_err(anyhow::Error::msg)?;
                Ok(project)
            });
        let mut project = match loaded {
            Ok(project) => project,
            Err(error) => {
                self.file_message = Some((true, format!("{error:#}")));
                return;
            }
        };
        make_project_external(&mut project.entries);
        let selected = first_scenario_path(&project.entries, &mut Vec::new());
        self.custom_project = Some(project);
        self.selected_project_scenario = selected.clone();
        self.selected_project_group = selected
            .as_ref()
            .map(|path| path[..path.len().saturating_sub(1)].to_vec())
            .unwrap_or_default();
        self.selected_step = selected.and_then(|path| {
            self.custom_project
                .as_ref()
                .and_then(|project| project.scenario(&path))
                .is_some_and(|task| !task.steps.is_empty())
                .then_some(0)
        });
        self.load_error = None;
        self.file_message = Some((false, format!("Проект загружен: {}", path.display())));
    }

    fn block_picker(&mut self, ctx: &egui::Context) {
        let Some(parent) = self.block_picker_parent.clone() else {
            return;
        };
        let picker_height = (ctx.content_rect().height() - 96.0).clamp(540.0, 760.0);
        let list_height = picker_height - 118.0;
        let mut selected = None;
        let mut close = false;
        egui::Modal::new(Id::new("composer-block-picker"))
            .frame(
                Frame::popup(&ctx.global_style())
                    .fill(surface(self.dark))
                    .corner_radius(14)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(560.0);
                ui.set_min_height(picker_height);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Добавить следующий блок")
                                .strong()
                                .size(20.0)
                                .color(text(self.dark)),
                        );
                        ui.label(
                            RichText::new(format!("Продолжение от: {parent}"))
                                .monospace()
                                .size(9.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Закрыть").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(12.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.block_picker_search)
                        .hint_text("Поиск доступного блока…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);
                let query = self.block_picker_search.trim().to_lowercase();
                ScrollArea::vertical()
                    .id_salt("composer-block-picker-list")
                    .max_height(list_height)
                    .show(ui, |ui| {
                        for kind in ComposerBlockKind::ALL {
                            let context = composer_output_context(kind);
                            if !query.is_empty()
                                && !kind.title().to_lowercase().contains(&query)
                                && !kind.category().to_lowercase().contains(&query)
                                && !context.to_lowercase().contains(&query)
                            {
                                continue;
                            }
                            let response = Frame::new()
                                .fill(panel(self.dark))
                                .stroke(Stroke::new(1.0, line(self.dark)))
                                .corner_radius(10)
                                .inner_margin(Margin::same(11))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(kind.title())
                                                .strong()
                                                .size(11.0)
                                                .color(text(self.dark)),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(kind.category())
                                                        .size(8.0)
                                                        .color(CYAN),
                                                );
                                            },
                                        );
                                    });
                                    ui.label(
                                        RichText::new(format!("Выход: {context}"))
                                            .monospace()
                                            .size(8.0)
                                            .color(PURPLE),
                                    );
                                })
                                .response
                                .interact(Sense::click());
                            if response.clicked() {
                                selected = Some(kind);
                            }
                            ui.add_space(6.0);
                        }
                    });
            });
        if close {
            self.block_picker_parent = None;
        } else if let Some(kind) = selected {
            self.add_composer_block(kind);
        }
    }
}

impl eframe::App for ScenarioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_run(ui.ctx());
        self.poll_github_authorization(ui.ctx());
        self.poll_github_repository_load(ui.ctx());
        self.top_bar(ui);
        self.left_library(ui);
        self.right_inspector(ui);
        self.canvas(ui);
        self.block_picker(ui.ctx());
        self.github_repository_picker(ui.ctx());
        self.run_confirmation(ui.ctx());
    }
}

fn project_group_entries_mut<'a>(
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

fn project_group_entries<'a>(
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

fn paint_project_group_tree(
    ui: &mut egui::Ui,
    entries: &[ProjectEntry],
    parent_path: &[usize],
    selected_group: &[usize],
    action: &mut Option<Vec<usize>>,
) {
    for (index, entry) in entries.iter().enumerate() {
        let ProjectEntry::Group {
            name,
            entries: children,
            ..
        } = entry
        else {
            continue;
        };
        let mut path = parent_path.to_vec();
        path.push(index);
        let selected = path == selected_group;
        let has_subgroups = children
            .iter()
            .any(|entry| matches!(entry, ProjectEntry::Group { .. }));
        let label = RichText::new(name).strong().size(9.0).color(if selected {
            PURPLE
        } else {
            ui.visuals().text_color()
        });

        if has_subgroups {
            let response = egui::CollapsingHeader::new(label)
                .id_salt(("project-group", path.clone()))
                .default_open(selected_group.starts_with(&path))
                .show(ui, |ui| {
                    paint_project_group_tree(ui, children, &path, selected_group, action);
                });
            if response.header_response.clicked() {
                *action = Some(path);
            }
        } else if ui.selectable_label(selected, label).clicked() {
            *action = Some(path);
        }
    }
}

fn validate_project(project: &ScenarioProject) -> Result<(), String> {
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

fn load_project_yaml(yaml: &str) -> anyhow::Result<ScenarioProject> {
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
            entries: vec![ProjectEntry::Scenario { task }],
        }],
    })
}

fn make_project_external(entries: &mut [ProjectEntry]) {
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

fn first_scenario_path(entries: &[ProjectEntry], prefix: &mut Vec<usize>) -> Option<Vec<usize>> {
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

impl ScenarioApp {
    fn top_bar(&mut self, root: &mut egui::Ui) {
        #[cfg(target_os = "macos")]
        let horizontal_margin = Margin {
            left: 84,
            right: 16,
            top: 10,
            bottom: 10,
        };
        #[cfg(not(target_os = "macos"))]
        let horizontal_margin = Margin::symmetric(16, 10);

        egui::Panel::top("topbar")
            .exact_size(68.0)
            .frame(
                Frame::new()
                    .fill(surface(self.dark))
                    .stroke(Stroke::new(1.0, line(self.dark)))
                    .inner_margin(horizontal_margin),
            )
            .show(root, |ui| {
                #[cfg(target_os = "macos")]
                {
                    let drag = ui.interact(
                        ui.max_rect(),
                        ui.id().with("native-titlebar-drag"),
                        Sense::drag(),
                    );
                    if drag.drag_started() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                }

                ui.horizontal(|ui| {
                    Frame::new()
                        .fill(if self.dark { Color32::WHITE } else { INK })
                        .corner_radius(10)
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.label(RichText::new("PP").strong().size(12.0).color(if self.dark {
                                INK
                            } else {
                                Color32::WHITE
                            }));
                        });
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("PPDUSTER")
                                .strong()
                                .size(12.0)
                                .color(text(self.dark)),
                        );
                        ui.label(RichText::new("SCENARIO FLOW").size(9.0).color(MUTED));
                    });

                    ui.add_space(36.0);
                    if let Some(task) = self.selected_task() {
                        let step_count = self
                            .task_pack
                            .as_ref()
                            .and_then(|pack| pack.resolve(&task.id).ok())
                            .map(|resolved| resolved.steps.len())
                            .unwrap_or(task.steps.len());
                        let structure = if task.is_template() {
                            format!(
                                "{} · {} сценариев · {} шагов",
                                task.id,
                                task.scenarios.len(),
                                step_count
                            )
                        } else {
                            format!("{} · {} шагов", task.id, step_count)
                        };
                        Frame::new()
                            .fill(panel(self.dark))
                            .stroke(Stroke::new(1.0, line(self.dark)))
                            .corner_radius(10)
                            .inner_margin(Margin::symmetric(14, 7))
                            .show(ui, |ui| {
                                ui.set_min_width(300.0);
                                ui.label(
                                    RichText::new(&task.name)
                                        .strong()
                                        .size(11.0)
                                        .color(text(self.dark)),
                                );
                                ui.label(RichText::new(structure).size(9.0).color(MUTED));
                            });
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button(if self.dark {
                                "☀  Светлая"
                            } else {
                                "☾  Тёмная"
                            })
                            .clicked()
                        {
                            self.dark = !self.dark;
                            configure_style(ui.ctx(), self.dark);
                        }
                        ui.label(RichText::new("SAFE MODE").strong().size(9.0).color(CYAN));
                    });
                });
            });
    }

    fn left_library(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("library")
            .exact_size(270.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(surface(self.dark))
                    .stroke(Stroke::new(1.0, line(self.dark)))
                    .inner_margin(Margin::same(14)),
            )
            .show(root, |ui| {
                ui.label(RichText::new("БИБЛИОТЕКА").strong().size(10.0).color(MUTED));
                ui.add_space(4.0);
                if self.custom_project.is_some() {
                    self.composer_palette(ui);
                    return;
                }
                ui.label(
                    RichText::new("Проект")
                        .strong()
                        .size(22.0)
                        .color(text(self.dark)),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "Откройте YAML проекта — группы в левой панели будут взяты из project.entries.",
                        )
                        .size(9.0)
                        .color(MUTED),
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        !self.running,
                        egui::Button::new("Открыть project YAML…")
                            .min_size(Vec2::new(ui.available_width(), 34.0)),
                    )
                    .clicked()
                {
                    self.load_scenario_file();
                    return;
                }
                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        !self.running,
                        egui::Button::new("＋ Новый проект")
                            .min_size(Vec2::new(ui.available_width(), 32.0)),
                    )
                    .clicked()
                {
                    self.start_custom_project();
                    return;
                }
                if let Some((is_error, message)) = &self.file_message {
                    ui.add_space(8.0);
                    ui.label(RichText::new(message).size(8.0).color(if *is_error {
                        ORANGE
                    } else {
                        CYAN
                    }));
                }
            });
    }

    fn composer_palette(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Конструктор")
                    .strong()
                    .size(22.0)
                    .color(text(self.dark)),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Закрыть").clicked() {
                    self.custom_project = None;
                    self.selected_project_scenario = None;
                    self.selected_project_group.clear();
                    self.selected_step = Some(0);
                    self.invalidate_plan();
                }
            });
        });
        if self.custom_project.is_none() {
            return;
        }
        let project_name = self
            .custom_project
            .as_ref()
            .map(|project| project.name.as_str())
            .unwrap_or("Проект");
        ui.label(RichText::new(project_name).size(9.0).color(MUTED));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.running, egui::Button::new("Загрузить…"))
                .clicked()
            {
                self.load_scenario_file();
            }
            if ui
                .add_enabled(!self.running, egui::Button::new("Сохранить…"))
                .clicked()
            {
                self.save_selected_scenario();
            }
        });
        if let Some((is_error, message)) = &self.file_message {
            ui.label(
                RichText::new(message)
                    .size(8.0)
                    .color(if *is_error { ORANGE } else { CYAN }),
            );
        }
        ui.add_space(8.0);
        section_label(ui, "ПРОЕКТ");
        ui.horizontal(|ui| {
            if ui.button("＋ Группа").clicked() {
                self.add_project_group();
            }
            if ui.button("＋ Сценарий").clicked() {
                self.add_project_scenario();
            }
        });
        if ui
            .add_enabled(
                !self.running,
                egui::Button::new("＋ GitHub · репозитории аккаунта")
                    .min_size(Vec2::new(ui.available_width(), 30.0)),
            )
            .clicked()
        {
            self.add_github_project_scenario();
        }
        ui.add_space(4.0);
        let project = self.custom_project.clone().expect("project checked above");
        let mut tree_action = None;
        ScrollArea::vertical()
            .id_salt("project-group-tree")
            .max_height(240.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                paint_project_group_tree(
                    ui,
                    &project.entries,
                    &[],
                    &self.selected_project_group,
                    &mut tree_action,
                );
            });
        if let Some(path) = tree_action {
            self.selected_project_group = path.clone();
            let selected_is_inside = self
                .selected_project_scenario
                .as_ref()
                .is_some_and(|selected| selected.starts_with(&path));
            if !selected_is_inside {
                if let Some(entries) = project_group_entries(&project, &path) {
                    let mut prefix = path;
                    self.selected_project_scenario = first_scenario_path(entries, &mut prefix);
                    self.selected_step = Some(0);
                    self.invalidate_plan();
                }
            }
        }
    }

    fn right_inspector(&mut self, root: &mut egui::Ui) {
        egui::Panel::right("inspector")
            .exact_size(360.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(surface(self.dark))
                    .stroke(Stroke::new(1.0, line(self.dark)))
                    .inner_margin(Margin::same(16)),
            )
            .show(root, |ui| {
                ui.label(RichText::new("ИНСПЕКТОР").strong().size(10.0).color(MUTED));
                ui.add_space(6.0);
                if self.custom_project.is_some() {
                    self.composer_inspector(ui);
                    return;
                }
                let Some(task) = self.selected_task().cloned() else {
                    if let Some(error) = &self.load_error {
                        error_box(ui, error, self.dark);
                    } else {
                        ui.label("Сценарии не найдены");
                    }
                    return;
                };
                let has_configurable_git_step = self
                    .task_pack
                    .as_ref()
                    .and_then(|pack| pack.resolve(&task.id).ok())
                    .is_some_and(|resolved| github_picker_source_steps(&resolved).is_some());
                let resolved = self.resolved_selected_task();
                let resolved_task = resolved.as_ref().ok().cloned();
                let resolution_error = resolved.err().map(|error| format!("{error:#}"));
                let preview_options = resolved_task
                    .as_ref()
                    .map(|resolved| self.options_for(resolved, false));
                let step_summaries = resolved_task
                    .as_ref()
                    .zip(preview_options.as_ref())
                    .map(|(resolved, options)| describe_task_steps(resolved, options))
                    .unwrap_or_default();
                let groups = self
                    .task_pack
                    .as_ref()
                    .zip(preview_options.as_ref())
                    .map(|(pack, options)| {
                        scenario_groups(pack, &task, options, resolved_task.as_ref())
                    })
                    .transpose()
                    .unwrap_or_else(|error| {
                        self.plan_error = Some(format!("{error:#}"));
                        None
                    })
                    .unwrap_or_default();
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.label(
                        RichText::new(&task.name)
                            .strong()
                            .size(18.0)
                            .color(text(self.dark)),
                    );
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new(&task.id)
                            .monospace()
                            .size(9.0)
                            .color(MUTED),
                    );
                    if task.is_template() {
                        ui.label(
                            RichText::new(format!(
                                "ШАБЛОН · {} сценариев · {} раскрытых шагов",
                                task.scenarios.len(),
                                resolved_task
                                    .as_ref()
                                    .map(|resolved| resolved.steps.len())
                                    .unwrap_or_default()
                            ))
                            .strong()
                            .size(9.0)
                            .color(PURPLE),
                        );
                    }
                    ui.add_space(10.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(if task.description.trim().is_empty() {
                                "Подробное описание для этого сценария пока не задано."
                            } else {
                                &task.description
                            })
                            .size(10.0)
                            .color(text(self.dark)),
                        )
                        .wrap(),
                    );
                    ui.add_space(14.0);

                    if let Some(error) = &resolution_error {
                        error_box(ui, error, self.dark);
                        ui.add_space(14.0);
                    }

                    if resolved_task
                        .as_ref()
                        .is_some_and(|resolved| resolved
                        .steps
                        .iter()
                        .any(|step| matches!(step.action, Action::BambuStudioRelease(_))))
                    {
                        section_label(ui, "КАНАЛ РЕЛИЗА");
                        let channel_before = self.channel;
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut self.channel,
                                ReleaseChannel::Release,
                                "Release",
                            );
                            ui.selectable_value(
                                &mut self.channel,
                                ReleaseChannel::Beta,
                                "Beta",
                            );
                        });
                        if self.channel != channel_before {
                            self.invalidate_plan();
                        }
                        ui.add_space(12.0);
                    }

                    if has_configurable_git_step {
                        section_label(ui, "РЕПОЗИТОРИИ GITHUB");
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    "Можно заменить одношаговый git-сценарий одним или несколькими публичными HTTPS-репозиториями из вашего GitHub.",
                                )
                                .size(9.0)
                                .color(MUTED),
                            )
                            .wrap(),
                        );
                        ui.add_space(6.0);
                        let selected_count = self.github_picker.selected_ids.len();
                        if ui
                            .add_enabled(
                                !self.running,
                                egui::Button::new(if selected_count == 0 {
                                    "Выбрать репозитории…".into()
                                } else {
                                    format!("Выбрано {selected_count} · изменить…")
                                })
                                .min_size(Vec2::new(ui.available_width(), 32.0)),
                            )
                            .clicked()
                        {
                            self.github_picker.open = true;
                            if self.github_picker.repositories.is_empty()
                                && !self.github_picker.loading
                            {
                                self.start_github_repository_load(ui.ctx());
                            }
                        }
                        if selected_count > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "Ветка каждого репозитория: main · папка: {}",
                                    self.github_picker.destination_root
                                ))
                                .size(8.0)
                                .color(PURPLE),
                            );
                        }
                        ui.add_space(12.0);
                    }

                    if task.is_template() {
                        section_label(ui, "СОСТАВ ШАБЛОНА");
                        for (index, group) in groups.iter().enumerate() {
                            Frame::new()
                                .fill(panel(self.dark))
                                .stroke(Stroke::new(1.0, line(self.dark)))
                                .corner_radius(9)
                                .inner_margin(Margin::same(9))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!("{:02}  {}", index + 1, group.name))
                                            .strong()
                                            .size(10.0)
                                            .color(text(self.dark)),
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {} шагов",
                                            group.id, group.step_count
                                        ))
                                        .monospace()
                                        .size(8.0)
                                        .color(PURPLE),
                                    );
                                });
                            ui.add_space(6.0);
                        }
                        ui.add_space(8.0);
                    }

                    section_label(ui, "ЧТО ПРОИЗОЙДЁТ");
                    if step_summaries.is_empty() {
                        ui.label(
                            RichText::new("Нет исполняемых шагов.")
                                .size(9.0)
                                .color(MUTED),
                        );
                    } else {
                        for (index, summary) in step_summaries.iter().enumerate() {
                            ui.horizontal_top(|ui| {
                                ui.label(
                                    RichText::new(format!("{:02}", index + 1))
                                        .monospace()
                                        .strong()
                                        .size(9.0)
                                        .color(PURPLE),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(summary)
                                            .size(9.0)
                                            .color(text(self.dark)),
                                    )
                                    .wrap(),
                                );
                            });
                            ui.add_space(4.0);
                        }
                    }
                    ui.add_space(14.0);

                    section_label(ui, "РАЗРЕШЕНИЯ");
                    let permissions_changed = ui
                        .checkbox(&mut self.allow_elevation, "Разрешить elevation")
                        .changed()
                        | ui.checkbox(
                            &mut self.allow_shell,
                            "Разрешить shell-команды и скрипты",
                        )
                        .changed();
                    if permissions_changed {
                        self.invalidate_plan();
                    }
                    ui.label(
                        RichText::new("Без этих флагов опасные шаги не попадут в план.")
                            .size(9.0)
                            .color(MUTED),
                    );
                    ui.add_space(14.0);

                    if ui
                        .add_enabled(
                            resolved_task.is_some() && !self.github_picker.loading,
                            egui::Button::new(
                                RichText::new("Проверить план")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .min_size(Vec2::new(ui.available_width(), 36.0))
                            .fill(PURPLE)
                            .corner_radius(9),
                        )
                        .clicked()
                    {
                        self.build_plan();
                    }
                    ui.add_space(7.0);
                    let can_run = self
                        .report
                        .as_ref()
                        .is_some_and(|report| report.errors.is_empty())
                        && !self.report_applied
                        && self.plan_error.is_none()
                        && !self.github_picker.loading
                        && !self.running
                        && github_selection_auth_ready(&self.github_picker)
                        && resolved_task.as_ref().is_some_and(task_supports_gui_run);
                    if ui
                        .add_enabled(
                            can_run,
                            egui::Button::new(if self.running {
                                "Выполняется…"
                            } else {
                                "Запустить сценарий"
                            })
                            .min_size(Vec2::new(ui.available_width(), 34.0))
                            .fill(ORANGE)
                            .corner_radius(9),
                        )
                        .clicked()
                    {
                        self.confirm_run = true;
                    }
                    if resolved_task
                        .as_ref()
                        .is_some_and(|resolved| !task_supports_gui_run(resolved))
                    {
                        let git_auth_missing = resolved_task
                            .as_ref()
                            .is_some_and(task_has_unready_git_credentials);
                        ui.label(
                            RichText::new(if git_auth_missing {
                                "Git credentials пока не готовы для фонового запуска. Настройте gh credential helper или SSH agent в окне выбора репозиториев."
                            } else {
                                "Этот сценарий требует терминала или vendor UI; используйте команду ниже."
                            })
                            .size(9.0)
                            .color(MUTED),
                        );
                    } else if !github_selection_auth_ready(&self.github_picker) {
                        ui.label(
                            RichText::new(
                                "Запуск заблокирован, пока Git credentials не готовы. Используйте подсказку в окне выбора репозиториев и затем снова проверьте план.",
                            )
                            .size(9.0)
                            .color(ORANGE),
                        );
                    }
                    ui.add_space(7.0);
                    if let Some(command) = self.command_for_selected() {
                        if ui
                            .add_sized(
                                [ui.available_width(), 32.0],
                                egui::Button::new("Скопировать команду запуска"),
                            )
                            .clicked()
                        {
                            ui.ctx().copy_text(command);
                        }
                    } else if !self.github_picker.selected_ids.is_empty() {
                        ui.label(
                            RichText::new(
                                "Выбранные GitHub-репозитории включены в план этого запуска в интерфейсе.",
                            )
                            .size(9.0)
                            .color(MUTED),
                        );
                    }

                    if let Some(error) = &self.plan_error {
                        ui.add_space(12.0);
                        error_box(ui, error, self.dark);
                    }
                    if let Some(report) = &self.report {
                        let failed = !report.errors.is_empty();
                        let report_color = if failed {
                            Color32::from_rgb(194, 64, 64)
                        } else {
                            CYAN
                        };
                        ui.add_space(12.0);
                        Frame::new()
                            .fill(translucent(
                                report_color,
                                if self.dark { 35 } else { 15 },
                            ))
                            .stroke(Stroke::new(1.0, translucent(report_color, 90)))
                            .corner_radius(10)
                            .inner_margin(Margin::same(10))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(if self.report_applied {
                                        if failed {
                                            format!(
                                                "Сценарий завершён с ошибкой · {} шагов",
                                                report.steps.len()
                                            )
                                        } else {
                                            format!(
                                                "Сценарий выполнен · {} шагов",
                                                report.steps.len()
                                            )
                                        }
                                    } else {
                                        format!("План готов · {} шагов", report.steps.len())
                                    })
                                    .strong()
                                    .color(report_color),
                                );
                                if self.report_applied {
                                    for step in &report.steps {
                                        let result = step
                                            .logs
                                            .last()
                                            .map(|log| log.message.as_str())
                                            .unwrap_or(&step.summary);
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "{}: {}",
                                                    step.step_name, result
                                                ))
                                                .size(9.0)
                                                .color(text(self.dark)),
                                            )
                                            .wrap(),
                                        );
                                    }
                                } else {
                                    ui.label(
                                        RichText::new("Никакие изменения не применены.")
                                            .size(9.0)
                                            .color(MUTED),
                                    );
                                }
                            });
                    }

                    ui.add_space(18.0);
                    if task.is_template() {
                        section_label(ui, "ВЫБРАННАЯ ГРУППА");
                        if let Some(group) = self
                            .selected_step
                            .and_then(|group_index| groups.get(group_index))
                        {
                            ui.label(
                                RichText::new(&group.name)
                                    .strong()
                                    .size(14.0)
                                    .color(text(self.dark)),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} раскрытых шагов",
                                    group.id, group.step_count
                                ))
                                    .monospace()
                                    .size(9.0)
                                    .color(MUTED),
                            );
                            ui.add_space(8.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&group.description)
                                        .size(9.0)
                                        .color(text(self.dark)),
                                )
                                .wrap(),
                            );
                            ui.add_space(8.0);
                            for summary in &group.step_summaries {
                                ui.horizontal_top(|ui| {
                                    ui.label(RichText::new("•").color(PURPLE));
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(summary)
                                                .size(9.0)
                                                .color(MUTED),
                                        )
                                        .wrap(),
                                    );
                                });
                            }
                        }
                    } else {
                        section_label(ui, "ВЫБРАННЫЙ ШАГ");
                        if let Some(step) = self.selected_step.and_then(|step_index| {
                            resolved_task
                                .as_ref()
                                .and_then(|resolved| resolved.steps.get(step_index))
                        }) {
                            paint_step_inspector(ui, step, preview_options.as_ref(), self.dark);
                        }
                    }
                });
            });
    }

    fn composer_inspector(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut move_to = None;
        let mut remove = None;
        let selected = self.selected_step;
        {
            let selected_path = self.selected_project_scenario.clone();
            let Some(task) = self
                .custom_project
                .as_mut()
                .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
            else {
                return;
            };
            ui.label(
                RichText::new("Пользовательский сценарий")
                    .strong()
                    .size(18.0)
                    .color(text(self.dark)),
            );
            ui.add_space(10.0);
            section_label(ui, "СЦЕНАРИЙ");
            ui.label(RichText::new("Название").size(9.0).color(MUTED));
            changed |= ui.text_edit_singleline(&mut task.name).changed();
            ui.label(RichText::new("ID").size(9.0).color(MUTED));
            changed |= ui.text_edit_singleline(&mut task.id).changed();
            ui.label(RichText::new("Описание").size(9.0).color(MUTED));
            changed |= ui
                .add(egui::TextEdit::multiline(&mut task.description).desired_rows(3))
                .changed();
            ui.add_space(12.0);

            section_label(ui, "ВЫБРАННЫЙ БЛОК");
            if let Some(index) = selected.filter(|index| *index < task.steps.len()) {
                let step_count = task.steps.len();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(index > 0, egui::Button::new("← Раньше"))
                        .clicked()
                    {
                        move_to = Some(index - 1);
                    }
                    if ui
                        .add_enabled(index + 1 < step_count, egui::Button::new("Позже →"))
                        .clicked()
                    {
                        move_to = Some(index + 1);
                    }
                    if ui.button("Удалить").clicked() {
                        remove = Some(index);
                    }
                });
                ui.add_space(8.0);
                changed |= paint_composer_step_editor(ui, &mut task.steps[index], self.dark);
                ui.add_space(12.0);
                section_label(ui, "ВЫХОДНОЙ КОНТЕКСТ");
                ui.label(
                    RichText::new(composer_step_output_context(&task.steps[index]))
                        .monospace()
                        .size(8.0)
                        .color(PURPLE),
                );
                ui.label(
                    RichText::new(
                        "Поля контекста доступны условиям и следующим блокам по ID этого блока.",
                    )
                    .size(8.0)
                    .color(MUTED),
                );
            } else {
                ui.label(
                    RichText::new("Выберите блок на канвасе или добавьте его из палитры слева.")
                        .size(9.0)
                        .color(MUTED),
                );
            }
        }
        if changed {
            self.invalidate_plan();
        }
        if let Some(target) = move_to {
            if let Some(index) = selected {
                self.move_composer_step(index, target);
            }
        }
        if let Some(index) = remove {
            self.remove_composer_step(index);
        }

        let is_github_repository_scenario = self
            .selected_task()
            .is_some_and(|task| github_picker_source_steps(task).is_some());
        if is_github_repository_scenario {
            ui.add_space(14.0);
            section_label(ui, "УЧЁТНАЯ ЗАПИСЬ GITHUB");
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Загрузите репозитории, доступные текущей сессии GitHub CLI, и выберите нужные для этого сценария.",
                    )
                    .size(9.0)
                    .color(MUTED),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            let selected_count = self.github_picker.selected_ids.len();
            if ui
                .add_enabled(
                    !self.running,
                    egui::Button::new(if selected_count == 0 {
                        "Получить список репозиториев…".into()
                    } else {
                        format!("Выбрано {selected_count} · изменить…")
                    })
                    .min_size(Vec2::new(ui.available_width(), 32.0)),
                )
                .clicked()
            {
                self.github_picker.open = true;
                if self.github_picker.repositories.is_empty() && !self.github_picker.loading {
                    self.start_github_repository_load(ui.ctx());
                }
            }
            if selected_count > 0 {
                ui.label(
                    RichText::new(format!(
                        "Ветка: main · папка: {}",
                        self.github_picker.destination_root
                    ))
                    .size(8.0)
                    .color(PURPLE),
                );
            }
        }

        ui.add_space(14.0);
        let validation = self
            .selected_task()
            .ok_or_else(|| "сценарий не выбран".to_owned())
            .and_then(|task| task.validate());
        match &validation {
            Ok(()) => ui.label(
                RichText::new("Сценарий корректен и готов к сохранению.")
                    .size(9.0)
                    .color(CYAN),
            ),
            Err(error) => ui.label(RichText::new(error).size(9.0).color(ORANGE)),
        };
        ui.add_space(8.0);
        if ui
            .add_enabled(
                validation.is_ok() && !self.running,
                egui::Button::new("Проверить план")
                    .min_size(Vec2::new(ui.available_width(), 34.0))
                    .fill(PURPLE),
            )
            .clicked()
        {
            self.build_plan();
        }
        let can_run = validation.is_ok()
            && self
                .report
                .as_ref()
                .is_some_and(|report| report.errors.is_empty())
            && !self.report_applied
            && !self.running;
        if ui
            .add_enabled(
                can_run,
                egui::Button::new("Запустить сценарий")
                    .min_size(Vec2::new(ui.available_width(), 34.0))
                    .fill(ORANGE),
            )
            .clicked()
        {
            self.confirm_run = true;
        }
        if let Some(error) = &self.plan_error {
            ui.add_space(8.0);
            error_box(ui, error, self.dark);
        } else if let Some(report) = &self.report {
            ui.add_space(8.0);
            ui.label(
                RichText::new(if self.report_applied {
                    format!("Выполнено шагов: {}", report.steps.len())
                } else {
                    format!("План готов: {} шагов", report.steps.len())
                })
                .size(9.0)
                .color(CYAN),
            );
        }
    }

    fn canvas(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(canvas(self.dark)))
            .show(root, |ui| {
                let Some(task) = self.selected_task().cloned() else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Нет доступных сценариев").color(MUTED));
                    });
                    return;
                };
                let resolved = match self.resolved_selected_task() {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        ui.centered_and_justified(|ui| {
                            error_box(ui, &format!("{error:#}"), self.dark);
                        });
                        return;
                    }
                };
                let options = self.options_for(&resolved, false);
                let groups = if task.is_template() {
                    match self
                        .task_pack
                        .as_ref()
                        .map(|pack| scenario_groups(pack, &task, &options, Some(&resolved)))
                        .transpose()
                    {
                        Ok(Some(groups)) => groups,
                        Ok(None) => Vec::new(),
                        Err(error) => {
                            ui.centered_and_justified(|ui| {
                                error_box(ui, &format!("{error:#}"), self.dark);
                            });
                            return;
                        }
                    }
                } else {
                    Vec::new()
                };
                let is_composer = self.custom_project.is_some();
                if is_composer {
                    self.ensure_composer_canvas(&task);
                }
                let composer_canvas = self
                    .custom_project
                    .as_ref()
                    .and_then(|project| project.canvases.get(&task.id))
                    .cloned();
                let node_count = if task.is_template() {
                    groups.len()
                } else {
                    resolved.steps.len() + usize::from(is_composer)
                };
                ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let node_size = if task.is_template() {
                            Vec2::new(258.0, 154.0)
                        } else {
                            Vec2::new(232.0, 116.0)
                        };
                        let node_stride = if task.is_template() { 318.0 } else { 286.0 };
                        let canvas_extent = composer_canvas.as_ref().map(|canvas| {
                            canvas
                                .positions
                                .values()
                                .fold(Vec2::new(0.0, 0.0), |extent, point| {
                                    Vec2::new(extent.x.max(point.x), extent.y.max(point.y))
                                })
                        });
                        let width = canvas_extent
                            .map(|extent| extent.x + node_size.x + 180.0)
                            .unwrap_or(node_count as f32 * node_stride + 180.0)
                            .max(ui.available_width());
                        let height = canvas_extent
                            .map(|extent| extent.y + node_size.y + 140.0)
                            .unwrap_or(690.0)
                            .max(690.0_f32.max(ui.available_height()));
                        let (response, painter) =
                            ui.allocate_painter(Vec2::new(width, height), Sense::drag());
                        let bounds = response.rect;
                        paint_grid(&painter, bounds, self.dark);

                        let positions = if let Some(canvas) = &composer_canvas {
                            std::iter::once("start")
                                .chain(resolved.steps.iter().map(|step| step.id.as_str()))
                                .filter_map(|id| canvas.positions.get(id))
                                .map(|point| bounds.min + Vec2::new(point.x, point.y))
                                .collect::<Vec<_>>()
                        } else {
                            (0..node_count)
                                .map(|index| {
                                    let x = bounds.left() + 80.0 + index as f32 * node_stride;
                                    let y =
                                        bounds.top() + 250.0 + ((index as f32 * 1.15).sin() * 78.0);
                                    Pos2::new(x, y)
                                })
                                .collect::<Vec<_>>()
                        };

                        if let Some(canvas) = &composer_canvas {
                            let position_map = std::iter::once("start")
                                .chain(resolved.steps.iter().map(|step| step.id.as_str()))
                                .zip(positions.iter().copied())
                                .map(|(id, position)| (id.to_owned(), position))
                                .collect::<BTreeMap<_, _>>();
                            paint_composer_connectors(
                                &painter,
                                &position_map,
                                &canvas.parents,
                                node_size,
                            );
                        } else {
                            paint_connectors(&painter, &positions, node_size);
                        }

                        if task.is_template() {
                            let mut report_offset = 0;
                            for (index, (group, position)) in
                                groups.iter().zip(positions.iter()).enumerate()
                            {
                                let rect = Rect::from_min_size(*position, node_size);
                                let interaction = ui.interact(
                                    rect,
                                    Id::new(("scenario-group", task.id.as_str(), index)),
                                    Sense::click(),
                                );
                                if interaction.clicked() {
                                    self.selected_step = Some(index);
                                }
                                let status = self.report.as_ref().and_then(|report| {
                                    aggregate_group_status(report, report_offset, group.step_count)
                                });
                                paint_group_node(
                                    &painter,
                                    rect,
                                    group,
                                    index,
                                    self.selected_step == Some(index),
                                    status.as_ref(),
                                    self.dark,
                                );
                                report_offset += group.step_count;
                            }
                        } else {
                            let step_positions = if is_composer {
                                if let Some(position) = positions.first() {
                                    let rect = Rect::from_min_size(*position, node_size);
                                    let drag = ui.interact(
                                        rect,
                                        Id::new(("scenario-start", task.id.as_str())),
                                        Sense::click_and_drag(),
                                    );
                                    if drag.dragged() {
                                        let delta = ui.ctx().input(|input| input.pointer.delta());
                                        self.drag_composer_node(&task.id, "start", delta);
                                    }
                                    painter.rect_filled(rect, 13.0, panel(self.dark));
                                    painter.rect_stroke(
                                        rect,
                                        13.0,
                                        Stroke::new(2.0, CYAN),
                                        StrokeKind::Inside,
                                    );
                                    painter.text(
                                        rect.left_top() + Vec2::new(18.0, 22.0),
                                        Align2::LEFT_TOP,
                                        "СТАРТ",
                                        FontId::proportional(10.0),
                                        CYAN,
                                    );
                                    painter.text(
                                        rect.left_top() + Vec2::new(18.0, 48.0),
                                        Align2::LEFT_TOP,
                                        "Начало сценария",
                                        FontId::proportional(17.0),
                                        text(self.dark),
                                    );
                                    painter.text(
                                        rect.left_top() + Vec2::new(18.0, 78.0),
                                        Align2::LEFT_TOP,
                                        "Контекст проекта",
                                        FontId::monospace(9.0),
                                        MUTED,
                                    );
                                    let plus_rect = Rect::from_center_size(
                                        Pos2::new(rect.right() - 20.0, rect.center().y),
                                        Vec2::splat(30.0),
                                    );
                                    painter.circle_filled(plus_rect.center(), 14.0, PURPLE);
                                    painter.text(
                                        plus_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "+",
                                        FontId::proportional(21.0),
                                        Color32::WHITE,
                                    );
                                    if ui
                                        .interact(
                                            plus_rect,
                                            Id::new(("scenario-start-plus", task.id.as_str())),
                                            Sense::click(),
                                        )
                                        .clicked()
                                    {
                                        self.open_block_picker("start");
                                    }
                                }
                                &positions[1..]
                            } else {
                                positions.as_slice()
                            };
                            for (index, (step, position)) in
                                resolved.steps.iter().zip(step_positions.iter()).enumerate()
                            {
                                let rect = Rect::from_min_size(*position, node_size);
                                let selected = self.selected_step == Some(index);
                                let interaction = ui.interact(
                                    rect,
                                    Id::new(("scenario-step", task.id.as_str(), index)),
                                    if is_composer {
                                        Sense::click_and_drag()
                                    } else {
                                        Sense::click()
                                    },
                                );
                                if interaction.clicked() {
                                    self.selected_step = Some(index);
                                }
                                if is_composer && interaction.dragged() {
                                    let delta = ui.ctx().input(|input| input.pointer.delta());
                                    self.drag_composer_node(&task.id, &step.id, delta);
                                }
                                paint_step_node(
                                    &painter,
                                    rect,
                                    step,
                                    index,
                                    selected,
                                    self.report
                                        .as_ref()
                                        .and_then(|report| report.steps.get(index))
                                        .map(|report| &report.status),
                                    self.dark,
                                );
                                if is_composer {
                                    let plus_rect = Rect::from_center_size(
                                        Pos2::new(rect.right() - 18.0, rect.center().y),
                                        Vec2::splat(28.0),
                                    );
                                    painter.circle_filled(plus_rect.center(), 12.0, PURPLE);
                                    painter.text(
                                        plus_rect.center(),
                                        Align2::CENTER_CENTER,
                                        "+",
                                        FontId::proportional(18.0),
                                        Color32::WHITE,
                                    );
                                    if ui
                                        .interact(
                                            plus_rect,
                                            Id::new(("scenario-step-plus", step.id.as_str())),
                                            Sense::click(),
                                        )
                                        .clicked()
                                    {
                                        self.open_block_picker(step.id.clone());
                                    }
                                }
                            }
                        }

                        painter.text(
                            Pos2::new(bounds.left() + 80.0, bounds.top() + 92.0),
                            Align2::LEFT_TOP,
                            if task.is_template() {
                                "ШАБЛОН СЦЕНАРИЯ"
                            } else {
                                "СЦЕНАРИЙ"
                            },
                            FontId::proportional(10.0),
                            MUTED,
                        );
                        painter.text(
                            Pos2::new(bounds.left() + 80.0, bounds.top() + 112.0),
                            Align2::LEFT_TOP,
                            &task.name,
                            FontId::proportional(26.0),
                            text(self.dark),
                        );
                        painter.text(
                            Pos2::new(bounds.left() + 80.0, bounds.top() + 150.0),
                            Align2::LEFT_TOP,
                            if task.is_template() {
                                format!(
                                    "{} групп · {} раскрытых шагов",
                                    groups.len(),
                                    resolved.steps.len()
                                )
                            } else {
                                format!("{} шагов", resolved.steps.len())
                            },
                            FontId::proportional(10.0),
                            MUTED,
                        );
                    });
            });
    }

    fn github_repository_picker(&mut self, ctx: &egui::Context) {
        if !self.github_picker.open {
            return;
        }

        let query = self.github_picker.search.trim().to_lowercase();
        let visible_repositories = self
            .github_picker
            .repositories
            .iter()
            .filter(|repository| {
                query.is_empty()
                    || repository.name_with_owner.to_lowercase().contains(&query)
                    || repository
                        .owner_name
                        .as_ref()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .cloned()
            .collect::<Vec<_>>();
        let missing_selected = self
            .github_picker
            .selected_ids
            .iter()
            .filter(|id| {
                !self
                    .github_picker
                    .repositories
                    .iter()
                    .any(|repository| &repository.id == *id)
            })
            .count();
        let mut configuration_changed = false;
        let mut request_refresh = false;
        let mut request_authorization = false;
        let mut close = false;

        egui::Modal::new(Id::new("github-repository-picker"))
            .frame(
                Frame::popup(&ctx.global_style())
                    .fill(surface(self.dark))
                    .corner_radius(14)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Репозитории GitHub")
                                .strong()
                                .size(20.0)
                                .color(text(self.dark)),
                        );
                        ui.label(
                            RichText::new(
                                "ppduster не запрашивает и не сохраняет токен; GitHub CLI может получить авторизацию из своей сессии или унаследованных переменных окружения.",
                            )
                            .size(9.0)
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !self.github_picker.loading && !self.github_picker.authorizing,
                                egui::Button::new(if self.github_picker.loaded_once {
                                    "Обновить"
                                } else {
                                    "Загрузить"
                                }),
                            )
                            .clicked()
                        {
                            request_refresh = true;
                        }
                        if self.github_picker.loading || self.github_picker.authorizing {
                            ui.spinner();
                        }
                    });
                });

                ui.add_space(14.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.github_picker.search)
                        .hint_text("Поиск по owner/repository…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);

                Frame::new()
                    .fill(panel(self.dark))
                    .stroke(Stroke::new(1.0, line(self.dark)))
                    .corner_radius(10)
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Корневая папка")
                                    .strong()
                                    .size(9.0)
                                    .color(text(self.dark)),
                            );
                            let root_response = ui.add_enabled(
                                !self.running,
                                egui::TextEdit::singleline(
                                    &mut self.github_picker.destination_root,
                                )
                                .desired_width(300.0),
                            );
                            configuration_changed |= root_response.changed();
                            ui.label(
                                RichText::new("PUBLIC HTTPS ONLY")
                                    .strong()
                                    .size(8.0)
                                    .color(PURPLE),
                            );
                        });
                        ui.label(
                            RichText::new(
                                "Путь: <корень>/<owner>/<repository>; синхронизируется main. Private и SSH отключены, чтобы фоновый git не запрашивал credentials.",
                            )
                            .size(8.0)
                            .color(MUTED),
                        );
                    });

                if let Some(error) = &self.github_picker.error {
                    ui.add_space(10.0);
                    error_box(ui, error, self.dark);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.github_picker.authorizing && !self.github_picker.loading,
                                egui::Button::new("Войти через GitHub"),
                            )
                            .clicked()
                        {
                            request_authorization = true;
                        }
                        if ui.button("Скопировать команду входа").clicked() {
                            ui.ctx().copy_text(
                                "gh auth login --hostname github.com --git-protocol https --web --clipboard"
                                    .into(),
                            );
                        }
                    });
                }

                ui.add_space(10.0);
                if self.github_picker.authorizing {
                    ui.vertical_centered(|ui| {
                        ui.add_space(30.0);
                        ui.spinner();
                        ui.label(
                            RichText::new("Ожидаю завершения входа в браузере…")
                                .color(MUTED),
                        );
                        ui.label(
                            RichText::new(
                                "Одноразовый код скопирован в буфер обмена. После входа список обновится автоматически.",
                            )
                            .size(9.0)
                            .color(MUTED),
                        );
                        ui.add_space(30.0);
                    });
                } else if self.github_picker.loading && self.github_picker.repositories.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(30.0);
                        ui.spinner();
                        ui.label(RichText::new("Получаю доступные репозитории…").color(MUTED));
                        ui.add_space(30.0);
                    });
                } else if self.github_picker.repositories.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.label(
                            RichText::new(if self.github_picker.loaded_once {
                                "Доступных репозиториев нет"
                            } else {
                                "Список пока не загружен"
                            })
                                .strong()
                                .color(text(self.dark)),
                        );
                        ui.label(
                            RichText::new(if self.github_picker.loaded_once {
                                "GitHub вернул пустой список для текущего аккаунта."
                            } else {
                                "Нужен установленный GitHub CLI. Войти можно прямо здесь."
                            })
                                .size(9.0)
                                .color(MUTED),
                        );
                        ui.add_space(24.0);
                    });
                } else {
                    ScrollArea::vertical()
                        .id_salt("github-repository-list")
                        .max_height(360.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if visible_repositories.is_empty() {
                                ui.label(
                                    RichText::new("По этому запросу ничего не найдено.")
                                        .color(MUTED),
                                );
                            }
                            for repository in &visible_repositories {
                                let mut selected =
                                    self.github_picker.selected_ids.contains(&repository.id);
                                let selectable = selected
                                    || (!repository.is_archived
                                        && !repository.is_private
                                        && repository.main_branch.is_some()
                                        && self.github_picker.selected_ids.len()
                                            < MAX_SELECTED_GITHUB_REPOSITORIES);
                                Frame::new()
                                    .fill(card(self.dark))
                                    .stroke(Stroke::new(1.0, line(self.dark)))
                                    .corner_radius(9)
                                    .inner_margin(Margin::symmetric(10, 8))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let response = ui.add_enabled(
                                                selectable && !self.running,
                                                egui::Checkbox::without_text(&mut selected),
                                            );
                                            if response.changed() {
                                                if selected {
                                                    self.github_picker
                                                        .selected_ids
                                                        .insert(repository.id.clone());
                                                } else {
                                                    self.github_picker
                                                        .selected_ids
                                                        .remove(&repository.id);
                                                }
                                                configuration_changed = true;
                                            }
                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(&repository.name_with_owner)
                                                        .strong()
                                                        .size(10.0)
                                                        .color(text(self.dark)),
                                                );
                                                let default_branch = repository
                                                    .default_branch
                                                    .as_deref()
                                                    .unwrap_or("нет default");
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} · default: {default_branch}{}{}",
                                                        if repository.main_branch.is_some() {
                                                            "main"
                                                        } else {
                                                            "нет main"
                                                        },
                                                        if repository.is_private {
                                                            " · PRIVATE"
                                                        } else {
                                                            " · PUBLIC"
                                                        },
                                                        if repository.is_archived {
                                                            " · ARCHIVED"
                                                        } else {
                                                            ""
                                                        }
                                                    ))
                                                    .monospace()
                                                    .size(8.0)
                                                    .color(if selectable { PURPLE } else { MUTED }),
                                                );
                                            });
                                        });
                                    });
                                ui.add_space(5.0);
                            }
                        });
                }

                if missing_selected > 0 {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "{missing_selected} ранее выбранных репозиториев больше не доступно; снимите выбор или обновите доступ."
                        ))
                        .size(9.0)
                        .color(ORANGE),
                    );
                }
                if self.github_picker.selected_ids.len() >= MAX_SELECTED_GITHUB_REPOSITORIES {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "Достигнут лимит: {} репозиториев за один сценарий.",
                            MAX_SELECTED_GITHUB_REPOSITORIES
                        ))
                        .size(9.0)
                        .color(ORANGE),
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Выбрано: {}",
                            self.github_picker.selected_ids.len()
                        ))
                        .strong()
                        .color(PURPLE),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Готово").strong().color(Color32::WHITE),
                                )
                                .fill(PURPLE),
                            )
                            .clicked()
                        {
                            close = true;
                        }
                        if ui
                            .add_enabled(
                                !self.github_picker.selected_ids.is_empty() && !self.running,
                                egui::Button::new("Сбросить выбор"),
                            )
                            .clicked()
                        {
                            self.github_picker.selected_ids.clear();
                            configuration_changed = true;
                        }
                    });
                });
            });

        if request_refresh {
            self.start_github_repository_load(ctx);
        }
        if request_authorization {
            self.start_github_authorization(ctx);
        }
        if configuration_changed {
            self.selected_step = Some(0);
            self.invalidate_plan();
        }
        if close {
            self.github_picker.open = false;
        }
    }

    fn run_confirmation(&mut self, ctx: &egui::Context) {
        if !self.confirm_run {
            return;
        }
        let task_name = self
            .selected_task()
            .map(|task| task.name.clone())
            .unwrap_or_default();
        egui::Modal::new(Id::new("confirm-scenario-run"))
            .frame(
                Frame::popup(&ctx.global_style())
                    .fill(surface(self.dark))
                    .corner_radius(14)
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(390.0);
                ui.label(
                    RichText::new("Применить сценарий?")
                        .strong()
                        .size(20.0)
                        .color(text(self.dark)),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(task_name)
                        .strong()
                        .size(12.0)
                        .color(PURPLE),
                );
                ui.label(
                    RichText::new(
                        "Будут выполнены шаги, показанные в проверенном плане. Окно останется открытым.",
                    )
                    .size(10.0)
                    .color(MUTED),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.button("Отмена").clicked() {
                        self.confirm_run = false;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Применить").strong().color(Color32::WHITE),
                            )
                            .fill(ORANGE),
                        )
                        .clicked()
                    {
                        self.start_run(ctx);
                    }
                });
            });
    }
}

const MAX_SELECTED_GITHUB_REPOSITORIES: usize = 200;

fn default_github_destination_root() -> String {
    dirs::home_dir()
        .map(|home| home.join("Developer").display().to_string())
        .unwrap_or_else(|| "$HOME/Developer".into())
}

fn github_repository_composer_task(ordinal: usize) -> Task {
    let steps = vec![composer_step(
        ComposerBlockKind::GithubListRepositories,
        "list-repositories".into(),
    )];
    Task {
        id: format!("github-repositories-{ordinal}"),
        name: "Получить репозитории GitHub".into(),
        description: "Получить логин текущей учётной записи GitHub CLI и массив полной метаинформации о доступных репозиториях.".into(),
        platform: ppduster::rules::Platform::Macos,
        trust: TrustRequirement::ExternalAllowed,
        scenarios: Vec::new(),
        resolved_scenarios: Vec::new(),
        steps,
    }
}

fn materialize_github_repositories(
    mut task: Task,
    repositories: &[GithubRepository],
    selected_ids: &BTreeSet<String>,
    destination_root: &str,
) -> anyhow::Result<Task> {
    if selected_ids.is_empty() {
        return Ok(task);
    }
    if selected_ids.len() > MAX_SELECTED_GITHUB_REPOSITORIES {
        anyhow::bail!(
            "за один запуск можно выбрать не более {} GitHub-репозиториев",
            MAX_SELECTED_GITHUB_REPOSITORIES
        );
    }

    let destination_root = destination_root.trim();
    if destination_root.is_empty() {
        anyhow::bail!("укажите корневую папку для GitHub-репозиториев");
    }
    if Path::new(destination_root)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("корневая папка GitHub не должна содержать '..'");
    }

    let source_steps = github_picker_source_steps(&task)
        .map(<[Step]>::to_vec)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "сценарий {} должен состоять из атомарных шагов git-inspect, git-clone-if-missing, git-fetch и git-fast-forward",
                task.id
            )
        })?;
    let mut selected = Vec::with_capacity(selected_ids.len());
    for id in selected_ids {
        let repository = repositories
            .iter()
            .find(|repository| &repository.id == id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ранее выбранный GitHub-репозиторий {} больше не доступен; обновите список и выбор",
                    id
                )
            })?;
        if repository.is_archived {
            anyhow::bail!(
                "архивный GitHub-репозиторий {} нельзя добавить в сценарий",
                repository.name_with_owner
            );
        }
        if repository.is_private {
            anyhow::bail!(
                "private GitHub-репозиторий {} нельзя запускать из GUI; picker поддерживает только публичный HTTPS",
                repository.name_with_owner
            );
        }
        selected.push(repository);
    }
    selected.sort_by(|left, right| {
        left.name_with_owner
            .to_lowercase()
            .cmp(&right.name_with_owner.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut generated_steps = Vec::with_capacity(selected.len() * source_steps.len());
    for repository in selected {
        validate_github_repository_identity(repository)?;
        let branch = repository.main_branch.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub-репозиторий {} не имеет ветки main",
                repository.name_with_owner
            )
        })?;
        if branch.trim().is_empty() {
            anyhow::bail!(
                "GitHub-репозиторий {} вернул пустое имя ветки main",
                repository.name_with_owner
            );
        }

        let slug = github_step_slug(repository);
        let repo = github_clone_url(repository);
        let dest = PathBuf::from(destination_root)
            .join(&repository.owner)
            .join(&repository.name)
            .display()
            .to_string();
        for source_step in &source_steps {
            let mut step = source_step.clone();
            step.id = format!("{}/{}", source_step.id, slug);
            step.name = match &source_step.action {
                Action::GitInspect { .. } => {
                    format!(
                        "Check whether {} exists locally",
                        repository.name_with_owner
                    )
                }
                Action::GitCloneIfMissing { .. } => {
                    format!("Clone {} when missing", repository.name_with_owner)
                }
                Action::GitFetch { .. } => {
                    format!("Fetch {} main", repository.name_with_owner)
                }
                Action::GitFastForward { .. } => {
                    format!("Fast-forward {} main", repository.name_with_owner)
                }
                _ => unreachable!("GitHub picker template was validated above"),
            };
            step.check = None;
            // The picker only materializes public github.com HTTPS URLs. No
            // credential prompt is needed in the background UI worker.
            step.auth = AuthPolicy::None;
            step.action = match &source_step.action {
                Action::GitInspect { .. } => Action::GitInspect {
                    repo: repo.clone(),
                    dest: dest.clone(),
                },
                Action::GitCloneIfMissing { .. } => Action::GitCloneIfMissing {
                    repo: repo.clone(),
                    dest: dest.clone(),
                    branch: Some(branch.to_owned()),
                },
                Action::GitFetch { .. } => Action::GitFetch {
                    repo: repo.clone(),
                    dest: dest.clone(),
                    branch: branch.to_owned(),
                },
                Action::GitFastForward { .. } => Action::GitFastForward {
                    repo: repo.clone(),
                    dest: dest.clone(),
                    branch: branch.to_owned(),
                },
                _ => unreachable!("GitHub picker template was validated above"),
            };
            generated_steps.push(step);
        }
    }

    task.steps.splice(.., generated_steps);
    task.validate().map_err(anyhow::Error::msg)?;
    Ok(task)
}

fn github_picker_source_steps(task: &Task) -> Option<&[Step]> {
    match task.steps.as_slice() {
        [inspect, clone, fetch, update]
            if matches!(inspect.action, Action::GitInspect { .. })
                && matches!(clone.action, Action::GitCloneIfMissing { .. })
                && matches!(fetch.action, Action::GitFetch { .. })
                && matches!(update.action, Action::GitFastForward { .. }) =>
        {
            Some(task.steps.as_slice())
        }
        _ => None,
    }
}

fn validate_github_repository_identity(repository: &GithubRepository) -> anyhow::Result<()> {
    let mut components = repository.name_with_owner.split('/');
    let owner = components.next().unwrap_or_default();
    let name = components.next().unwrap_or_default();
    if components.next().is_some()
        || owner != repository.owner
        || name != repository.name
        || !is_safe_github_component(owner)
        || !is_safe_github_component(name)
    {
        anyhow::bail!(
            "GitHub вернул недопустимое имя репозитория {}",
            repository.name_with_owner
        );
    }
    Ok(())
}

fn is_safe_github_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
}

fn github_clone_url(repository: &GithubRepository) -> String {
    format!("https://github.com/{}.git", repository.name_with_owner)
}

fn github_step_slug(repository: &GithubRepository) -> String {
    let name_with_owner = &repository.name_with_owner;
    let mut slug = String::with_capacity(name_with_owner.len());
    let mut previous_dash = false;
    for character in name_with_owner.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    let digest = Sha256::digest(format!("{}\0{}", repository.id, repository.name_with_owner));
    format!("{}-{}", slug.trim_matches('-'), hex::encode(&digest[..16]))
}

fn composer_block_id(kind: ComposerBlockKind) -> &'static str {
    match kind {
        ComposerBlockKind::GithubListRepositories => "list-github-repositories",
        ComposerBlockKind::GitInspect => "inspect-repository",
        ComposerBlockKind::GitCloneIfMissing => "clone-repository",
        ComposerBlockKind::GitFetch => "fetch-repository",
        ComposerBlockKind::GitFastForward => "update-branch",
        ComposerBlockKind::CreateDirectory => "create-directory",
        ComposerBlockKind::InspectPath => "inspect-path",
        ComposerBlockKind::CopyPath => "copy-path",
        ComposerBlockKind::WriteFile => "write-file",
        ComposerBlockKind::RemovePath => "remove-path",
        ComposerBlockKind::BrewInstall => "install-package",
    }
}

fn composer_output_context(kind: ComposerBlockKind) -> &'static str {
    match kind {
        ComposerBlockKind::GithubListRepositories => {
            "github.account.login, github.repositories[]: { id, owner, name, full_name, https_url, ssh_url, default_branch, private, archived }"
        }
        ComposerBlockKind::GitInspect => {
            "repository.exists, repository.path, repository.remote_url"
        }
        ComposerBlockKind::GitCloneIfMissing => {
            "repository.path, repository.remote_url, repository.branch, repository.cloned"
        }
        ComposerBlockKind::GitFetch => {
            "repository.path, repository.remote_url, repository.branch, repository.fetched"
        }
        ComposerBlockKind::GitFastForward => {
            "repository.path, repository.branch, repository.updated"
        }
        ComposerBlockKind::CreateDirectory => "path.value, path.created",
        ComposerBlockKind::InspectPath => {
            "path.value, path.exists, path.kind, path.size_bytes, path.sha256"
        }
        ComposerBlockKind::CopyPath => "path.source, path.destination, path.copied",
        ComposerBlockKind::WriteFile => "file.path, file.bytes, file.changed",
        ComposerBlockKind::RemovePath => "path.value, path.removed",
        ComposerBlockKind::BrewInstall => "package.name, package.cask, package.installed",
    }
}

fn composer_step_output_context(step: &Step) -> &'static str {
    match &step.action {
        Action::GithubListRepositories => {
            composer_output_context(ComposerBlockKind::GithubListRepositories)
        }
        Action::GitInspect { .. } => composer_output_context(ComposerBlockKind::GitInspect),
        Action::GitCloneIfMissing { .. } => {
            composer_output_context(ComposerBlockKind::GitCloneIfMissing)
        }
        Action::GitFetch { .. } => composer_output_context(ComposerBlockKind::GitFetch),
        Action::GitFastForward { .. } => composer_output_context(ComposerBlockKind::GitFastForward),
        Action::CreateDirectory(_) => composer_output_context(ComposerBlockKind::CreateDirectory),
        Action::InspectPath(_) => composer_output_context(ComposerBlockKind::InspectPath),
        Action::CopyPath(_) => composer_output_context(ComposerBlockKind::CopyPath),
        Action::WriteFile(_) => composer_output_context(ComposerBlockKind::WriteFile),
        Action::RemovePath(_) => composer_output_context(ComposerBlockKind::RemovePath),
        Action::BrewInstall { .. } => composer_output_context(ComposerBlockKind::BrewInstall),
        _ => "result.status, result.summary",
    }
}

fn composer_step(kind: ComposerBlockKind, id: String) -> Step {
    let repository = "https://github.com/owner/repository.git".to_owned();
    let destination = "$HOME/Developer/owner/repository".to_owned();
    let action = match kind {
        ComposerBlockKind::GithubListRepositories => Action::GithubListRepositories,
        ComposerBlockKind::GitInspect => Action::GitInspect {
            repo: repository,
            dest: destination,
        },
        ComposerBlockKind::GitCloneIfMissing => Action::GitCloneIfMissing {
            repo: repository,
            dest: destination,
            branch: Some("main".into()),
        },
        ComposerBlockKind::GitFetch => Action::GitFetch {
            repo: repository,
            dest: destination,
            branch: "main".into(),
        },
        ComposerBlockKind::GitFastForward => Action::GitFastForward {
            repo: repository,
            dest: destination,
            branch: "main".into(),
        },
        ComposerBlockKind::CreateDirectory => Action::CreateDirectory(CreateDirectoryAction {
            path: "$HOME/Developer/project".into(),
        }),
        ComposerBlockKind::InspectPath => Action::InspectPath(InspectPathAction {
            path: "$HOME/Developer/project".into(),
            recursive_size: false,
            sha256: false,
            expect: None,
        }),
        ComposerBlockKind::CopyPath => Action::CopyPath(CopyPathAction {
            src: "$HOME/Developer/source".into(),
            dest: "$HOME/Developer/destination".into(),
        }),
        ComposerBlockKind::WriteFile => Action::WriteFile(WriteFileAction {
            path: "$HOME/Developer/project/example.txt".into(),
            content: String::new(),
            on_conflict: WriteConflictPolicy::Fail,
        }),
        ComposerBlockKind::RemovePath => Action::RemovePath(RemovePathAction {
            path: "$HOME/Library/Caches/example".into(),
        }),
        ComposerBlockKind::BrewInstall => Action::BrewInstall {
            package: "ripgrep".into(),
            cask: false,
        },
    };
    Step {
        id,
        name: kind.title().into(),
        auth: AuthPolicy::None,
        check: None,
        dangerous: false,
        allow_elevation: Default::default(),
        when: None,
        require: None,
        action,
    }
}

fn describe_task_steps(task: &Task, options: &RunOptions) -> Vec<String> {
    task.steps
        .iter()
        .map(|step| {
            describe_step(step, options)
                .unwrap_or_else(|error| format!("{}: не удалось описать шаг: {error:#}", step.id))
        })
        .collect()
}

fn scenario_groups(
    pack: &TaskPack,
    template: &Task,
    options: &RunOptions,
    configured: Option<&Task>,
) -> anyhow::Result<Vec<ScenarioGroup>> {
    let mut groups = template
        .scenarios
        .iter()
        .map(|scenario_id| {
            let scenario = pack.get(scenario_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "шаблон {} ссылается на неизвестный сценарий {}",
                    template.id,
                    scenario_id
                )
            })?;
            let resolved = pack.resolve(scenario_id)?;
            Ok(ScenarioGroup {
                id: scenario.id.clone(),
                name: scenario.name.clone(),
                description: if scenario.description.trim().is_empty() {
                    "Подробное описание для этой группы пока не задано.".into()
                } else {
                    scenario.description.clone()
                },
                step_count: resolved.steps.len(),
                step_summaries: describe_task_steps(&resolved, options),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if !template.is_template() {
        return Ok(groups);
    }

    if let Some(configured) = configured {
        let base = pack.resolve(&template.id)?;
        if configured.steps.len() != base.steps.len() {
            let source_step_id = base
                .steps
                .iter()
                .find(|step| {
                    matches!(
                        step.action,
                        Action::GitInspect { .. }
                            | Action::GitCloneIfMissing { .. }
                            | Action::GitFetch { .. }
                            | Action::GitFastForward { .. }
                    )
                })
                .map(|step| step.id.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "configured template {} changed step count without a git source step",
                        template.id
                    )
                })?;
            let group = groups
                .iter_mut()
                .find(|group| {
                    source_step_id == group.id
                        || source_step_id
                            .strip_prefix(&group.id)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "configured git step {} is outside direct groups of template {}",
                        source_step_id,
                        template.id
                    )
                })?;
            group.step_count = group
                .step_count
                .checked_add(configured.steps.len() - base.steps.len())
                .ok_or_else(|| anyhow::anyhow!("configured scenario group is too large"))?;
        }

        let mut offset = 0usize;
        for group in &mut groups {
            let end = offset
                .checked_add(group.step_count)
                .ok_or_else(|| anyhow::anyhow!("configured scenario group offset overflow"))?;
            let steps = configured.steps.get(offset..end).ok_or_else(|| {
                anyhow::anyhow!(
                    "configured task {} does not match scenario group {}",
                    configured.id,
                    group.id
                )
            })?;
            group.step_summaries = steps
                .iter()
                .map(|step| {
                    describe_step(step, options).unwrap_or_else(|error| {
                        format!("{}: не удалось описать шаг: {error:#}", step.id)
                    })
                })
                .collect();
            offset = end;
        }
        if offset != configured.steps.len() {
            anyhow::bail!(
                "configured task {} has {} ungrouped step(s)",
                configured.id,
                configured.steps.len() - offset
            );
        }
    }

    Ok(groups)
}

fn paint_step_inspector(ui: &mut egui::Ui, step: &Step, options: Option<&RunOptions>, dark: bool) {
    ui.label(
        RichText::new(step_title(step))
            .strong()
            .size(14.0)
            .color(text(dark)),
    );
    ui.label(RichText::new(&step.id).monospace().size(9.0).color(MUTED));
    if let Some(options) = options {
        let summary = describe_step(step, options)
            .unwrap_or_else(|error| format!("Не удалось описать шаг: {error:#}"));
        ui.add_space(8.0);
        ui.add(egui::Label::new(RichText::new(summary).size(9.0).color(PURPLE)).wrap());
    }
    ui.add_space(8.0);
    let yaml = serde_yaml::to_string(step).unwrap_or_else(|error| format!("Ошибка: {error}"));
    Frame::new()
        .fill(code_surface(dark))
        .corner_radius(8)
        .inner_margin(Margin::same(9))
        .show(ui, |ui| {
            ui.label(RichText::new(yaml).monospace().size(9.0).color(text(dark)));
        });
}

fn paint_composer_step_editor(ui: &mut egui::Ui, step: &mut Step, dark: bool) -> bool {
    let mut changed = false;
    let is_git_fetch = matches!(&step.action, Action::GitFetch { .. });
    ui.label(RichText::new("Название блока").size(9.0).color(MUTED));
    changed |= ui.text_edit_singleline(&mut step.name).changed();
    ui.label(RichText::new("ID блока").size(9.0).color(MUTED));
    changed |= ui.text_edit_singleline(&mut step.id).changed();
    ui.add_space(8.0);
    match &mut step.action {
        Action::GithubListRepositories => {
            ui.label(
                RichText::new(
                    "Блок использует текущую учётную запись GitHub CLI и не требует параметров.",
                )
                .size(9.0)
                .color(MUTED),
            );
        }
        Action::GitInspect { repo, dest } => {
            changed |= composer_text_field(ui, "Repository URL", repo);
            changed |= composer_text_field(ui, "Локальная папка", dest);
        }
        Action::GitCloneIfMissing { repo, dest, branch } => {
            changed |= composer_text_field(ui, "Repository URL", repo);
            changed |= composer_text_field(ui, "Локальная папка", dest);
            changed |=
                composer_text_field(ui, "Ветка", branch.get_or_insert_with(|| "main".into()));
            changed |= composer_git_auth(ui, &mut step.auth);
        }
        Action::GitFetch { repo, dest, branch } | Action::GitFastForward { repo, dest, branch } => {
            changed |= composer_text_field(ui, "Repository URL", repo);
            changed |= composer_text_field(ui, "Локальная папка", dest);
            changed |= composer_text_field(ui, "Ветка", branch);
            if is_git_fetch {
                changed |= composer_git_auth(ui, &mut step.auth);
            }
        }
        Action::CreateDirectory(action) => {
            changed |= composer_text_field(ui, "Путь", &mut action.path);
        }
        Action::InspectPath(action) => {
            changed |= composer_text_field(ui, "Путь", &mut action.path);
            changed |= ui
                .checkbox(&mut action.recursive_size, "Рекурсивно считать размер")
                .changed();
            changed |= ui
                .checkbox(&mut action.sha256, "Вычислить SHA-256")
                .changed();
        }
        Action::CopyPath(action) => {
            changed |= composer_text_field(ui, "Источник", &mut action.src);
            changed |= composer_text_field(ui, "Назначение", &mut action.dest);
        }
        Action::WriteFile(action) => {
            changed |= composer_text_field(ui, "Путь", &mut action.path);
            ui.label(RichText::new("Содержимое").size(9.0).color(MUTED));
            changed |= ui
                .add(egui::TextEdit::multiline(&mut action.content).desired_rows(5))
                .changed();
            let mut replace = matches!(action.on_conflict, WriteConflictPolicy::Replace);
            if ui
                .checkbox(&mut replace, "Заменять отличающийся файл")
                .changed()
            {
                action.on_conflict = if replace {
                    WriteConflictPolicy::Replace
                } else {
                    WriteConflictPolicy::Fail
                };
                changed = true;
            }
        }
        Action::RemovePath(action) => {
            changed |= composer_text_field(ui, "Путь", &mut action.path);
        }
        Action::BrewInstall { package, cask } => {
            changed |= composer_text_field(ui, "Пакет", package);
            changed |= ui.checkbox(cask, "Cask").changed();
        }
        _ => {
            ui.label(
                RichText::new("Редактор параметров для этого типа блока пока недоступен.")
                    .size(9.0)
                    .color(ORANGE),
            );
        }
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new("Изменения сразу отражаются на канвасе и в сохраняемом YAML.")
            .size(8.0)
            .color(if changed { PURPLE } else { text(dark) }),
    );
    changed
}

fn composer_text_field(ui: &mut egui::Ui, label: &str, value: &mut String) -> bool {
    ui.label(RichText::new(label).size(9.0).color(MUTED));
    ui.text_edit_singleline(value).changed()
}

fn composer_git_auth(ui: &mut egui::Ui, auth: &mut AuthPolicy) -> bool {
    let mut enabled = matches!(auth, AuthPolicy::GitCredential);
    if ui
        .checkbox(&mut enabled, "Использовать Git credentials")
        .changed()
    {
        *auth = if enabled {
            AuthPolicy::GitCredential
        } else {
            AuthPolicy::None
        };
        true
    } else {
        false
    }
}

fn branch_offset(sibling_index: usize) -> f32 {
    if sibling_index == 0 {
        return 0.0;
    }
    let distance = sibling_index.div_ceil(2) as f32 * 158.0;
    if sibling_index % 2 == 1 {
        distance
    } else {
        -distance
    }
}

fn paint_connector(painter: &egui::Painter, from: Pos2, to: Pos2) {
    let bend = ((to.x - from.x).abs() * 0.46).max(34.0);
    let direction = if to.x >= from.x { 1.0 } else { -1.0 };
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [
            from,
            from + Vec2::new(bend * direction, 0.0),
            to - Vec2::new(bend * direction, 0.0),
            to,
        ],
        false,
        Color32::TRANSPARENT,
        Stroke::new(4.0, translucent(PURPLE, 115)),
    ));
    painter.circle_filled(from, 7.0, PURPLE);
    painter.circle_filled(to, 7.0, PURPLE);
    painter.circle_stroke(from, 11.0, Stroke::new(2.0, translucent(PURPLE, 80)));
    painter.circle_stroke(to, 11.0, Stroke::new(2.0, translucent(PURPLE, 80)));
}

fn paint_connectors(painter: &egui::Painter, positions: &[Pos2], node_size: Vec2) {
    for pair in positions.windows(2) {
        let from = pair[0] + Vec2::new(node_size.x, node_size.y * 0.5);
        let to = pair[1] + Vec2::new(0.0, node_size.y * 0.5);
        paint_connector(painter, from, to);
    }
}

fn paint_composer_connectors(
    painter: &egui::Painter,
    positions: &BTreeMap<String, Pos2>,
    parents: &BTreeMap<String, String>,
    node_size: Vec2,
) {
    for (child, parent) in parents {
        let (Some(from), Some(to)) = (positions.get(parent), positions.get(child)) else {
            continue;
        };
        paint_connector(
            painter,
            *from + Vec2::new(node_size.x, node_size.y * 0.5),
            *to + Vec2::new(0.0, node_size.y * 0.5),
        );
    }
}

fn paint_group_node(
    painter: &egui::Painter,
    rect: Rect,
    group: &ScenarioGroup,
    index: usize,
    selected: bool,
    status: Option<&StepStatus>,
    dark: bool,
) {
    let accent = PURPLE;
    let shadow = rect.translate(Vec2::new(0.0, 7.0));
    painter.rect_filled(
        shadow,
        CornerRadius::same(14),
        translucent(Color32::BLACK, 22),
    );
    painter.rect(
        rect,
        CornerRadius::same(14),
        card(dark),
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected { PURPLE } else { line(dark) },
        ),
        StrokeKind::Outside,
    );
    painter.rect_filled(
        Rect::from_min_max(rect.min, Pos2::new(rect.left() + 7.0, rect.bottom())),
        CornerRadius::same(14),
        accent,
    );

    let icon_rect = Rect::from_min_size(rect.min + Vec2::new(20.0, 18.0), Vec2::new(38.0, 38.0));
    painter.rect_filled(
        icon_rect,
        CornerRadius::same(9),
        translucent(accent, if dark { 54 } else { 28 }),
    );
    painter.text(
        icon_rect.center(),
        Align2::CENTER_CENTER,
        "◇",
        FontId::proportional(18.0),
        accent,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 16.0),
        Align2::LEFT_TOP,
        "СЦЕНАРИЙ-ГРУППА",
        FontId::proportional(8.0),
        MUTED,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 34.0),
        Align2::LEFT_TOP,
        truncate(&group.name, 25),
        FontId::proportional(13.0),
        text(dark),
    );

    for (line_index, line) in wrap_text(&group.description, 38, 2).iter().enumerate() {
        painter.text(
            rect.min + Vec2::new(20.0, 68.0 + line_index as f32 * 15.0),
            Align2::LEFT_TOP,
            line,
            FontId::proportional(9.0),
            MUTED,
        );
    }
    painter.text(
        rect.min + Vec2::new(20.0, 111.0),
        Align2::LEFT_TOP,
        format!("{:02}  {}", index + 1, truncate(&group.id, 27)),
        FontId::monospace(9.0),
        MUTED,
    );
    painter.text(
        rect.min + Vec2::new(20.0, 132.0),
        Align2::LEFT_TOP,
        format!("{} раскрытых шагов", group.step_count),
        FontId::proportional(8.0),
        PURPLE,
    );
    paint_status_badge(painter, rect, status);
}

fn aggregate_group_status(report: &RunReport, start: usize, count: usize) -> Option<StepStatus> {
    let statuses = report.steps.get(start..start.checked_add(count)?)?;
    if statuses.is_empty() {
        return None;
    }
    if statuses
        .iter()
        .any(|step| matches!(&step.status, StepStatus::Failed))
    {
        Some(StepStatus::Failed)
    } else if statuses
        .iter()
        .any(|step| matches!(&step.status, StepStatus::Running))
    {
        Some(StepStatus::Running)
    } else if statuses
        .iter()
        .any(|step| matches!(&step.status, StepStatus::WaitingForAttention))
    {
        Some(StepStatus::WaitingForAttention)
    } else if statuses
        .iter()
        .all(|step| matches!(&step.status, StepStatus::Satisfied))
    {
        Some(StepStatus::Satisfied)
    } else if statuses
        .iter()
        .all(|step| matches!(&step.status, StepStatus::Applied | StepStatus::Satisfied))
    {
        Some(StepStatus::Applied)
    } else if statuses
        .iter()
        .all(|step| matches!(&step.status, StepStatus::Skipped))
    {
        Some(StepStatus::Skipped)
    } else {
        Some(StepStatus::Pending)
    }
}

fn wrap_text(value: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let extra = usize::from(!current.is_empty());
        if !current.is_empty() && current.chars().count() + extra + word.chars().count() > max_chars
        {
            lines.push(current);
            current = String::new();
            if lines.len() == max_lines {
                break;
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    if lines.len() == max_lines
        && value.split_whitespace().count()
            > lines
                .iter()
                .map(|line| line.split_whitespace().count())
                .sum::<usize>()
    {
        if let Some(last) = lines.last_mut() {
            let trimmed = last.trim_end_matches('…');
            *last = format!("{}…", truncate(trimmed, max_chars.saturating_sub(1)));
        }
    }
    lines
}

fn paint_step_node(
    painter: &egui::Painter,
    rect: Rect,
    step: &Step,
    index: usize,
    selected: bool,
    status: Option<&StepStatus>,
    dark: bool,
) {
    let accent = action_color(&step.action);
    let shadow = rect.translate(Vec2::new(0.0, 7.0));
    painter.rect_filled(
        shadow,
        CornerRadius::same(14),
        translucent(Color32::BLACK, 22),
    );
    painter.rect(
        rect,
        CornerRadius::same(14),
        card(dark),
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected { PURPLE } else { line(dark) },
        ),
        StrokeKind::Outside,
    );
    painter.rect_filled(
        Rect::from_min_max(rect.min, Pos2::new(rect.left() + 7.0, rect.bottom())),
        CornerRadius::same(14),
        accent,
    );

    let icon_rect = Rect::from_min_size(rect.min + Vec2::new(20.0, 20.0), Vec2::new(38.0, 38.0));
    painter.rect_filled(
        icon_rect,
        CornerRadius::same(9),
        translucent(accent, if dark { 54 } else { 28 }),
    );
    painter.text(
        icon_rect.center(),
        Align2::CENTER_CENTER,
        action_icon(&step.action),
        FontId::proportional(14.0),
        accent,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 18.0),
        Align2::LEFT_TOP,
        action_eyebrow(&step.action).to_uppercase(),
        FontId::proportional(8.0),
        MUTED,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 36.0),
        Align2::LEFT_TOP,
        truncate(&step_title(step), 23),
        FontId::proportional(13.0),
        text(dark),
    );
    painter.text(
        rect.min + Vec2::new(20.0, 78.0),
        Align2::LEFT_TOP,
        format!("{:02}  {}", index + 1, truncate(&step.id, 27)),
        FontId::monospace(9.0),
        MUTED,
    );
    paint_status_badge(painter, rect, status);
}

fn paint_status_badge(painter: &egui::Painter, rect: Rect, status: Option<&StepStatus>) {
    let (status_text, status_color) = match status {
        Some(StepStatus::Satisfied) => ("ГОТОВО", CYAN),
        Some(StepStatus::Failed) => ("ОШИБКА", Color32::from_rgb(194, 64, 64)),
        Some(StepStatus::Applied) => ("ВЫПОЛНЕНО", CYAN),
        Some(StepStatus::Skipped) => ("ПРОПУЩЕНО", MUTED),
        Some(StepStatus::Running) => ("ВЫПОЛНЯЕТСЯ", ORANGE),
        Some(StepStatus::WaitingForAttention) => ("ОЖИДАЕТ ВВОД", ORANGE),
        Some(StepStatus::Pending) | None => ("ОЖИДАЕТ", PURPLE),
    };
    painter.circle_filled(rect.max - Vec2::new(22.0, 19.0), 4.0, status_color);
    painter.text(
        rect.max - Vec2::new(32.0, 24.0),
        Align2::RIGHT_TOP,
        status_text,
        FontId::proportional(7.0),
        status_color,
    );
}

fn paint_grid(painter: &egui::Painter, rect: Rect, dark: bool) {
    let grid = if dark {
        Color32::from_rgba_unmultiplied(255, 255, 255, 12)
    } else {
        Color32::from_rgba_unmultiplied(70, 67, 58, 14)
    };
    let step = 32.0;
    let mut x = rect.left();
    while x <= rect.right() {
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, grid),
        );
        x += step;
    }
    let mut y = rect.top();
    while y <= rect.bottom() {
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.0, grid),
        );
        y += step;
    }
}

fn action_color(action: &Action) -> Color32 {
    match action {
        Action::GithubListRepositories => PURPLE,
        Action::CreateDirectory(_) | Action::InspectPath(_) | Action::WriteFile(_) => CYAN,
        Action::CopyPath(_)
        | Action::DownloadFile { .. }
        | Action::GitClone { .. }
        | Action::GitCloneIfMissing { .. }
        | Action::GitFetch { .. }
        | Action::GitFastForward { .. } => PURPLE,
        Action::GitInspect { .. } => CYAN,
        Action::RemovePath(_)
        | Action::ExtractArchive { .. }
        | Action::InstallDmg { .. }
        | Action::InstallPkg { .. } => ORANGE,
        Action::MacosRequirements { .. } => CYAN,
        Action::BrewInstall { .. } | Action::AppStoreInstall(_) => BLUE,
        Action::RunCommand { .. } | Action::RunScript { .. } => Color32::from_rgb(139, 95, 191),
        Action::BambuStudioRelease(_) => ORANGE,
        Action::ActivateLicense(_) => Color32::from_rgb(183, 90, 115),
        Action::ConfigurePackageRegistryFiles { .. } => CYAN,
    }
}

fn action_icon(action: &Action) -> &'static str {
    match action {
        Action::GithubListRepositories => "GH",
        Action::CreateDirectory(_) => "+DIR",
        Action::InspectPath(_) => "INFO",
        Action::CopyPath(_) => "COPY",
        Action::WriteFile(_) => "TXT",
        Action::RemovePath(_) => "DEL",
        Action::GitClone { .. } | Action::GitCloneIfMissing { .. } => "⌘",
        Action::GitInspect { .. } => "G?",
        Action::GitFetch { .. } => "↓G",
        Action::GitFastForward { .. } => "FF",
        Action::BrewInstall { .. } => "B",
        Action::RunCommand { .. } => ">_",
        Action::RunScript { interpreter, .. } => match interpreter {
            ScriptInterpreter::Sh => "SH",
            ScriptInterpreter::Bash => "#!",
            ScriptInterpreter::PowerShell => "PS",
        },
        Action::DownloadFile { .. } => "↓",
        Action::ExtractArchive { .. } => "▣",
        Action::InstallDmg { .. } | Action::InstallPkg { .. } => "APP",
        Action::MacosRequirements { .. } => "✓",
        Action::AppStoreInstall(_) => "A",
        Action::BambuStudioRelease(_) => "3D",
        Action::ActivateLicense(_) => "KEY",
        Action::ConfigurePackageRegistryFiles { .. } => "REG",
    }
}

fn action_eyebrow(action: &Action) -> &'static str {
    match action {
        Action::GithubListRepositories => "Репозитории GitHub",
        Action::CreateDirectory(_) => "Папка",
        Action::InspectPath(_) => "Метаданные",
        Action::CopyPath(_) => "Копирование",
        Action::WriteFile(_) => "Запись файла",
        Action::RemovePath(_) => "Корзина",
        Action::GitClone { .. } | Action::GitCloneIfMissing { .. } => "Клонирование",
        Action::GitInspect { .. } => "Проверка Git",
        Action::GitFetch { .. } => "Получение Git",
        Action::GitFastForward { .. } => "Актуализация Git",
        Action::BrewInstall { .. } | Action::AppStoreInstall(_) => "Пакет",
        Action::RunCommand { .. } => "Команда",
        Action::RunScript { interpreter, .. } => match interpreter {
            ScriptInterpreter::Sh => "sh-скрипт",
            ScriptInterpreter::Bash => "Bash-скрипт",
            ScriptInterpreter::PowerShell => "PowerShell-скрипт",
        },
        Action::DownloadFile { .. } => "Загрузка",
        Action::ExtractArchive { .. } => "Распаковка",
        Action::InstallDmg { .. } | Action::InstallPkg { .. } => "Установка",
        Action::MacosRequirements { .. } => "Проверка",
        Action::BambuStudioRelease(_) => "Релиз",
        Action::ActivateLicense(_) => "Активация",
        Action::ConfigurePackageRegistryFiles { .. } => "Реестр пакетов",
    }
}

fn step_title(step: &Step) -> String {
    if step.name.trim().is_empty() {
        step.id.clone()
    } else {
        step.name.clone()
    }
}

fn task_supports_gui_run(task: &Task) -> bool {
    task.steps
        .iter()
        .all(|step| matches!(step.auth, AuthPolicy::None) && action_supports_gui_run(&step.action))
}

fn git_clone_auth_ready(repo: &str) -> bool {
    if repo.starts_with("git@") || repo.starts_with("ssh://") {
        if std::env::var_os("SSH_AUTH_SOCK").is_none() {
            return false;
        }
        let ssh_keygen = if Path::new("/usr/bin/ssh-keygen").is_file() {
            "/usr/bin/ssh-keygen"
        } else {
            "ssh-keygen"
        };
        return Command::new(ssh_keygen)
            .args(["-F", "github.com"])
            .output()
            .map(|output| {
                output.status.success()
                    && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
            })
            .unwrap_or(false);
    }
    Command::new("git")
        .args([
            "config",
            "--get-urlmatch",
            "credential.helper",
            "https://github.com",
        ])
        .output()
        .map(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
        .unwrap_or(false)
}

fn github_selection_auth_ready(picker: &GithubPickerState) -> bool {
    !picker
        .repositories
        .iter()
        .any(|repository| repository.is_private && picker.selected_ids.contains(&repository.id))
}

fn task_has_unready_git_credentials(task: &Task) -> bool {
    task.steps.iter().any(|step| {
        matches!(step.auth, AuthPolicy::GitCredential)
            && matches!(
                &step.action,
                Action::GitClone { repo, .. }
                    | Action::GitInspect { repo, .. }
                    | Action::GitCloneIfMissing { repo, .. }
                    | Action::GitFetch { repo, .. }
                    | Action::GitFastForward { repo, .. }
                    if !git_clone_auth_ready(repo)
            )
    })
}

fn action_supports_gui_run(action: &Action) -> bool {
    match action {
        Action::ActivateLicense(_)
        | Action::AppStoreInstall(_)
        | Action::RunScript { .. }
        | Action::ConfigurePackageRegistryFiles { .. } => false,
        Action::GithubListRepositories
        | Action::CreateDirectory(_)
        | Action::InspectPath(_)
        | Action::CopyPath(_)
        | Action::WriteFile(_)
        | Action::RemovePath(_)
        | Action::GitClone { .. }
        | Action::GitInspect { .. }
        | Action::GitCloneIfMissing { .. }
        | Action::GitFetch { .. }
        | Action::GitFastForward { .. }
        | Action::BrewInstall { .. }
        | Action::RunCommand { .. }
        | Action::DownloadFile { .. }
        | Action::ExtractArchive { .. }
        | Action::InstallDmg { .. }
        | Action::InstallPkg { .. }
        | Action::MacosRequirements { .. }
        | Action::BambuStudioRelease(_) => true,
    }
}

fn truncate(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let result = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn section_label(ui: &mut egui::Ui, label: &str) {
    ui.label(RichText::new(label).strong().size(9.0).color(MUTED));
    ui.add_space(5.0);
}

fn error_box(ui: &mut egui::Ui, error: &str, dark: bool) {
    let red = Color32::from_rgb(194, 64, 64);
    Frame::new()
        .fill(translucent(red, if dark { 36 } else { 16 }))
        .stroke(Stroke::new(1.0, translucent(red, 95)))
        .corner_radius(9)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(error).size(9.0).color(red));
        });
}

fn load_tasks() -> anyhow::Result<TaskPack> {
    load_tasks_with_files(&[])
}

fn load_tasks_with_files(imported_files: &[PathBuf]) -> anyhow::Result<TaskPack> {
    let mut candidates = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tasks")];
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("tasks"));
        }
    }
    let mut sources = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for path in candidates {
        if !path.is_dir() {
            continue;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        if seen.insert(canonical.clone()) {
            sources.push(TaskSource {
                path: canonical,
                trust: PackTrust::Bundled,
            });
        }
    }
    sources.extend(imported_files.iter().cloned().map(|path| TaskSource {
        path,
        trust: PackTrust::External,
    }));
    TaskPack::load_many_with_overrides(&sources, true)
}

fn configure_style(ctx: &egui::Context, dark: bool) {
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_theme(theme);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = surface(dark);
    visuals.window_fill = surface(dark);
    visuals.extreme_bg_color = code_surface(dark);
    visuals.faint_bg_color = panel(dark);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);
    visuals.selection.bg_fill = translucent(PURPLE, 70);
    visuals.selection.stroke = Stroke::new(1.0, PURPLE);
    ctx.set_visuals_of(theme, visuals);

    let mut style = (*ctx.style_of(theme)).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(10.0, 7.0);
    ctx.set_style_of(theme, style);
}

fn translucent(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn surface(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(24, 28, 30)
    } else {
        CARD
    }
}

fn panel(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(31, 36, 38)
    } else {
        Color32::from_rgb(249, 249, 245)
    }
}

fn canvas(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(21, 25, 27)
    } else {
        PAPER
    }
}

fn card(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(37, 42, 44)
    } else {
        CARD
    }
}

fn code_surface(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(18, 22, 24)
    } else {
        Color32::from_rgb(242, 242, 237)
    }
}

fn text(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(232, 234, 229)
    } else {
        INK
    }
}

fn line(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(54, 60, 61)
    } else {
        LINE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_scenarios_are_available_to_the_ui() {
        let pack = load_tasks().unwrap();
        assert!(pack.get("bambu-studio-install").is_some());
        assert!(pack.get("lightburn-install-activate").is_some());
        assert!(pack.get("macos-developer-workstation").is_some());
    }

    #[test]
    fn gui_execution_excludes_flows_that_need_external_context() {
        let pack = load_tasks().unwrap();
        assert!(task_supports_gui_run(
            &pack.resolve("bambu-studio-install").unwrap()
        ));
        assert!(!task_supports_gui_run(
            &pack.resolve("lightburn-install-activate").unwrap()
        ));
        assert!(task_supports_gui_run(
            &pack.resolve("app-store-bootstrap").unwrap()
        ));
        assert!(!task_supports_gui_run(
            pack.get("dev-dodopizza-package-registries").unwrap()
        ));
    }

    #[test]
    fn template_canvas_uses_direct_scenario_groups() {
        let pack = load_tasks().unwrap();
        let template = pack.get("macos-developer-workstation").unwrap();
        assert!(template.is_template());

        let resolved = pack.resolve(&template.id).unwrap();
        let groups =
            scenario_groups(&pack, template, &RunOptions::default(), Some(&resolved)).unwrap();

        assert_eq!(groups.len(), template.scenarios.len());
        assert_eq!(
            groups.iter().map(|group| group.step_count).sum::<usize>(),
            resolved.steps.len()
        );
        assert!(groups.iter().all(|group| !group.description.is_empty()));
        assert!(groups
            .iter()
            .all(|group| group.step_summaries.len() == group.step_count));
        assert!(resolved.steps.len() > groups.len());
    }

    #[test]
    fn inspector_describes_every_resolved_step() {
        let pack = load_tasks().unwrap();
        let resolved = pack.resolve("macos-developer-workstation").unwrap();
        let summaries = describe_task_steps(&resolved, &RunOptions::default());

        assert_eq!(summaries.len(), resolved.steps.len());
        assert!(summaries.iter().all(|summary| !summary.trim().is_empty()));
    }

    #[test]
    fn github_selection_expands_to_atomic_git_steps_per_repository() {
        let pack = load_tasks().unwrap();
        let task = standalone_github_picker_task(&pack);
        let repositories = vec![
            github_repository("R2", "zeta/api", "trunk"),
            github_repository("R1", "acme/api", "main"),
        ];
        let selected_ids = BTreeSet::from(["R2".to_owned(), "R1".to_owned()]);

        let configured =
            materialize_github_repositories(task, &repositories, &selected_ids, "/tmp/workspaces")
                .unwrap();

        assert_eq!(configured.steps.len(), 8);
        assert!(configured.steps[0]
            .id
            .starts_with("inspect-repository/acme-api-"));
        assert!(configured.steps[4]
            .id
            .starts_with("inspect-repository/zeta-api-"));
        assert_ne!(configured.steps[0].id, configured.steps[1].id);
        assert!(configured
            .steps
            .iter()
            .all(|step| matches!(step.auth, AuthPolicy::None)));
        assert!(matches!(
            &configured.steps[0].action,
            Action::GitInspect { repo, dest }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
        ));
        assert!(matches!(
            &configured.steps[1].action,
            Action::GitCloneIfMissing { repo, dest, branch }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch.as_deref() == Some("main")
        ));
        assert!(matches!(
            &configured.steps[2].action,
            Action::GitFetch { repo, dest, branch }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch == "main"
        ));
        assert!(matches!(
            &configured.steps[3].action,
            Action::GitFastForward { repo, dest, branch }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch == "main"
        ));

        let report = run_task(&configured, &RunOptions::default()).unwrap();
        assert_eq!(report.steps.len(), 8);
        assert!(report.steps[0].summary.contains("acme/api"));
        assert!(report.steps[4].summary.contains("zeta/api"));
    }

    #[test]
    fn github_selection_rejects_missing_branch_and_path_traversal() {
        let pack = load_tasks().unwrap();
        let task = standalone_github_picker_task(&pack);
        let mut missing_branch = github_repository("R1", "acme/empty", "main");
        missing_branch.main_branch = None;
        let selected_ids = BTreeSet::from(["R1".to_owned()]);
        let error = materialize_github_repositories(
            task.clone(),
            &[missing_branch],
            &selected_ids,
            "/tmp/workspaces",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ветки main"));

        let traversal = github_repository("R1", "../escape", "main");
        let error =
            materialize_github_repositories(task, &[traversal], &selected_ids, "/tmp/workspaces")
                .unwrap_err()
                .to_string();
        assert!(error.contains("недопустимое имя"));
    }

    #[test]
    fn github_selection_rejects_private_repositories_and_downstream_steps() {
        let pack = load_tasks().unwrap();
        let selected_ids = BTreeSet::from(["R1".to_owned()]);
        let mut private = github_repository("R1", "acme/private", "main");
        private.is_private = true;
        let error = materialize_github_repositories(
            standalone_github_picker_task(&pack),
            &[private],
            &selected_ids,
            "/tmp/workspaces",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("private"));
        assert!(error.contains("публичный HTTPS"));

        let task_with_downstream_step = pack.resolve("dev-brew-bootstrap").unwrap();
        assert!(github_picker_source_steps(&task_with_downstream_step).is_none());
        let public = github_repository("R1", "acme/public", "main");
        let error = materialize_github_repositories(
            task_with_downstream_step,
            &[public],
            &selected_ids,
            "/tmp/workspaces",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("атомарных шагов"));
    }

    #[test]
    fn github_generated_step_ids_are_stable_and_resist_slug_collisions() {
        let dotted = github_repository("R_dotted", "acme/foo.bar", "main");
        let dashed = github_repository("R_dashed", "acme/foo-bar", "main");

        let dotted_id = github_step_slug(&dotted);
        assert_eq!(dotted_id, github_step_slug(&dotted));
        assert!(dotted_id.starts_with("acme-foo-bar-"));
        assert!(github_step_slug(&dashed).starts_with("acme-foo-bar-"));
        assert_ne!(dotted_id, github_step_slug(&dashed));
    }

    #[test]
    fn composer_builds_and_round_trips_atomic_git_blocks() {
        let mut task = Task {
            id: "custom-git-sync".into(),
            name: "Custom Git sync".into(),
            description: "A custom scenario assembled from atomic Git blocks.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: Vec::new(),
        };
        for (index, kind) in [
            ComposerBlockKind::GitInspect,
            ComposerBlockKind::GitCloneIfMissing,
            ComposerBlockKind::GitFetch,
            ComposerBlockKind::GitFastForward,
        ]
        .into_iter()
        .enumerate()
        {
            task.steps
                .push(composer_step(kind, format!("step-{}", index + 1)));
        }

        task.validate().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        let reparsed: TaskFile = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reparsed.task.steps.len(), 4);
        assert!(matches!(
            reparsed.task.steps[0].action,
            Action::GitInspect { .. }
        ));
        assert!(matches!(
            reparsed.task.steps[1].action,
            Action::GitCloneIfMissing { .. }
        ));
        assert!(matches!(
            reparsed.task.steps[2].action,
            Action::GitFetch { .. }
        ));
        assert!(matches!(
            reparsed.task.steps[3].action,
            Action::GitFastForward { .. }
        ));
    }

    #[test]
    fn project_round_trips_nested_groups_and_selects_first_scenario() {
        let task = Task {
            id: "nested-scenario".into(),
            name: "Nested scenario".into(),
            description: "A scenario stored below two project groups.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![composer_step(
                ComposerBlockKind::GitInspect,
                "inspect".into(),
            )],
        };
        let project = ScenarioProject {
            id: "workstation".into(),
            name: "Workstation".into(),
            description: "Developer workstation project.".into(),
            canvases: BTreeMap::new(),
            entries: vec![ProjectEntry::Group {
                id: "git".into(),
                name: "Git".into(),
                entries: vec![ProjectEntry::Group {
                    id: "repositories".into(),
                    name: "Repositories".into(),
                    entries: vec![ProjectEntry::Scenario { task }],
                }],
            }],
        };

        validate_project(&project).unwrap();
        let yaml = serde_yaml::to_string(&ScenarioProjectFile { project }).unwrap();
        let reparsed = load_project_yaml(&yaml).unwrap();
        let path = first_scenario_path(&reparsed.entries, &mut Vec::new()).unwrap();

        assert_eq!(path, vec![0, 0, 0]);
        assert_eq!(reparsed.scenario(&path).unwrap().id, "nested-scenario");
    }

    #[test]
    fn project_round_trips_canvas_positions_and_multiple_children() {
        let task = Task {
            id: "branched-scenario".into(),
            name: "Branched scenario".into(),
            description: "Two blocks attached to Start.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![
                composer_step(ComposerBlockKind::InspectPath, "inspect-a".into()),
                composer_step(ComposerBlockKind::InspectPath, "inspect-b".into()),
            ],
        };
        let project = ScenarioProject {
            id: "branched-project".into(),
            name: "Branched project".into(),
            description: String::new(),
            entries: vec![ProjectEntry::Scenario { task }],
            canvases: BTreeMap::from([(
                "branched-scenario".into(),
                ComposerCanvas {
                    positions: BTreeMap::from([
                        ("start".into(), CanvasPoint { x: 80.0, y: 250.0 }),
                        ("inspect-a".into(), CanvasPoint { x: 366.0, y: 170.0 }),
                        ("inspect-b".into(), CanvasPoint { x: 366.0, y: 330.0 }),
                    ]),
                    parents: BTreeMap::from([
                        ("inspect-a".into(), "start".into()),
                        ("inspect-b".into(), "start".into()),
                    ]),
                },
            )]),
        };

        let yaml = serde_yaml::to_string(&ScenarioProjectFile { project }).unwrap();
        let reparsed = load_project_yaml(&yaml).unwrap();
        let canvas = &reparsed.canvases["branched-scenario"];

        assert_eq!(canvas.parents["inspect-a"], "start");
        assert_eq!(canvas.parents["inspect-b"], "start");
        assert_eq!(canvas.positions["inspect-b"].y, 330.0);
    }

    #[test]
    fn project_yaml_drives_nested_group_tree() {
        let yaml = r#"
project:
  id: workstation
  name: Workstation
  entries:
    - type: group
      id: development
      name: Development
      entries:
        - type: group
          id: git
          name: Git
          entries: []
"#;
        let project = load_project_yaml(yaml).unwrap();

        let nested = project_group_entries(&project, &[0]).unwrap();
        assert!(matches!(
            nested.first(),
            Some(ProjectEntry::Group { id, name, .. }) if id == "git" && name == "Git"
        ));
    }

    #[test]
    fn project_loader_wraps_legacy_single_scenario_files() {
        let task = Task {
            id: "legacy".into(),
            name: "Legacy".into(),
            description: "A legacy standalone scenario.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            steps: vec![composer_step(
                ComposerBlockKind::CreateDirectory,
                "create".into(),
            )],
        };
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        let project = load_project_yaml(&yaml).unwrap();
        let path = first_scenario_path(&project.entries, &mut Vec::new()).unwrap();

        assert_eq!(project.scenario(&path).unwrap().id, "legacy");
    }

    #[test]
    fn composer_blocks_publish_searchable_output_context_contracts() {
        assert_eq!(
            composer_output_context(ComposerBlockKind::GitInspect),
            "repository.exists, repository.path, repository.remote_url"
        );
        assert!(composer_output_context(ComposerBlockKind::InspectPath).contains("path.sha256"));
        assert!(
            composer_output_context(ComposerBlockKind::GitCloneIfMissing)
                .contains("repository.cloned")
        );
        for kind in ComposerBlockKind::ALL {
            assert!(!composer_output_context(kind).trim().is_empty());
        }
    }

    #[test]
    fn github_composer_scenario_publishes_repository_array_contract() {
        let task = github_repository_composer_task(3);

        assert_eq!(task.id, "github-repositories-3");
        assert_eq!(task.name, "Получить репозитории GitHub");
        assert_eq!(task.steps.len(), 1);
        assert!(matches!(
            task.steps[0].action,
            Action::GithubListRepositories
        ));
        assert_eq!(
            composer_step_output_context(&task.steps[0]),
            "github.account.login, github.repositories[]: { id, owner, name, full_name, https_url, ssh_url, default_branch, private, archived }"
        );
        assert!(task
            .steps
            .iter()
            .all(|step| matches!(step.auth, AuthPolicy::None)));
        task.validate().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        assert!(yaml.contains("type: github-list-repositories"));
        let round_trip: TaskFile = serde_yaml::from_str(&yaml).unwrap();
        assert!(matches!(
            round_trip.task.steps[0].action,
            Action::GithubListRepositories
        ));
    }

    fn standalone_github_picker_task(pack: &TaskPack) -> Task {
        pack.resolve("github-repositories").unwrap()
    }

    fn github_repository(
        id: &str,
        name_with_owner: &str,
        default_branch: &str,
    ) -> GithubRepository {
        let (owner, name) = name_with_owner.split_once('/').unwrap();
        GithubRepository {
            id: id.into(),
            name: name.into(),
            name_with_owner: name_with_owner.into(),
            url: format!("https://github.com/{name_with_owner}"),
            ssh_url: format!("git@github.com:{name_with_owner}.git"),
            is_private: false,
            is_archived: false,
            default_branch: Some(default_branch.into()),
            main_branch: Some("main".into()),
            owner: owner.into(),
            owner_name: None,
        }
    }
}
