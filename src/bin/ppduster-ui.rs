use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::PackTrust;
use ppduster::automation::{
    describe_step, run_task, Action, AuthPolicy, ReleaseChannel, RunOptions, RunReport,
    ScriptInterpreter, Step, StepStatus, Task, TaskPack, TaskSource,
};
use ppduster::github::{list_accessible_repositories, GithubRepository};
use std::collections::BTreeSet;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubCloneProtocol {
    Https,
    Ssh,
}

struct GithubPickerState {
    open: bool,
    search: String,
    destination_root: String,
    protocol: GithubCloneProtocol,
    repositories: Vec<GithubRepository>,
    selected_ids: BTreeSet<String>,
    loaded_once: bool,
    loading: bool,
    error: Option<String>,
    receiver: Option<Receiver<Result<Vec<GithubRepository>, String>>>,
}

impl Default for GithubPickerState {
    fn default() -> Self {
        Self {
            open: false,
            search: String::new(),
            destination_root: default_github_destination_root(),
            protocol: GithubCloneProtocol::Https,
            repositories: Vec::new(),
            selected_ids: BTreeSet::new(),
            loaded_once: false,
            loading: false,
            error: None,
            receiver: None,
        }
    }
}

fn main() -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("ppduster · Scenario Flow")
        .with_inner_size([1440.0, 900.0])
        .with_min_inner_size([980.0, 680.0]);
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
    search: String,
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
                    .position(|task| task.id == "bambu-studio-install")
            })
            .unwrap_or(0);
        Self {
            task_pack,
            load_error,
            selected_task,
            selected_step: Some(0),
            search: String::new(),
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
        }
    }

    fn selected_task(&self) -> Option<&Task> {
        self.task_pack.as_ref()?.tasks.get(self.selected_task)
    }

    fn resolved_selected_task(&self) -> anyhow::Result<Task> {
        let task = self
            .selected_task()
            .ok_or_else(|| anyhow::anyhow!("сценарий не выбран"))?;
        let resolved = self
            .task_pack
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("библиотека сценариев не загружена"))?
            .resolve(&task.id)?;
        materialize_github_repositories(
            resolved,
            &self.github_picker.repositories,
            &self.github_picker.selected_ids,
            &self.github_picker.destination_root,
            self.github_picker.protocol,
        )
    }

    fn invalidate_plan(&mut self) {
        self.report = None;
        self.report_applied = false;
        self.plan_error = None;
        self.confirm_run = false;
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

    fn select_task(&mut self, index: usize) {
        if self.running {
            return;
        }
        self.selected_task = index;
        self.selected_step = Some(0);
        self.github_picker.open = false;
        self.github_picker.selected_ids.clear();
        self.github_picker.search.clear();
        self.invalidate_plan();
    }

    fn command_for_selected(&self) -> Option<String> {
        if !self.github_picker.selected_ids.is_empty() {
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
}

impl eframe::App for ScenarioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_run(ui.ctx());
        self.poll_github_repository_load(ui.ctx());
        self.top_bar(ui);
        self.left_library(ui);
        self.right_inspector(ui);
        self.canvas(ui);
        self.github_repository_picker(ui.ctx());
        self.run_confirmation(ui.ctx());
    }
}

impl ScenarioApp {
    fn top_bar(&mut self, root: &mut egui::Ui) {
        egui::Panel::top("topbar")
            .exact_size(68.0)
            .frame(
                Frame::new()
                    .fill(surface(self.dark))
                    .stroke(Stroke::new(1.0, line(self.dark)))
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(root, |ui| {
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
                ui.label(
                    RichText::new("Сценарии")
                        .strong()
                        .size(22.0)
                        .color(text(self.dark)),
                );
                ui.add_space(10.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Поиск сценария…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);
                ui.separator();

                let query = self.search.trim().to_lowercase();
                let visible = self
                    .task_pack
                    .as_ref()
                    .map(|pack| {
                        pack.tasks
                            .iter()
                            .enumerate()
                            .filter(|(_, task)| task_matches_query(task, &query))
                            .map(|(index, task)| {
                                (
                                    index,
                                    task.name.clone(),
                                    task.id.clone(),
                                    pack.resolve(&task.id)
                                        .map(|resolved| resolved.steps.len())
                                        .unwrap_or(task.steps.len()),
                                    task.platform.as_str().to_string(),
                                    task.is_template(),
                                    task.scenarios.len(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(8.0);
                        for (index, name, id, step_count, platform, is_template, scenario_count) in
                            visible
                        {
                            let selected = index == self.selected_task;
                            let response = Frame::new()
                                .fill(if selected {
                                    translucent(PURPLE, if self.dark { 42 } else { 18 })
                                } else {
                                    panel(self.dark)
                                })
                                .stroke(Stroke::new(
                                    1.0,
                                    if selected { PURPLE } else { line(self.dark) },
                                ))
                                .corner_radius(11)
                                .inner_margin(Margin::symmetric(11, 10))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(name)
                                                    .strong()
                                                    .size(11.0)
                                                    .color(text(self.dark)),
                                            );
                                            ui.label(
                                                RichText::new(id)
                                                    .monospace()
                                                    .size(8.0)
                                                    .color(MUTED),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(format!("{step_count}"))
                                                        .strong()
                                                        .size(10.0)
                                                        .color(if selected {
                                                            PURPLE
                                                        } else {
                                                            MUTED
                                                        }),
                                                );
                                            },
                                        );
                                    });
                                    ui.add_space(5.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(platform.to_uppercase())
                                                .size(8.0)
                                                .color(CYAN),
                                        );
                                        if is_template {
                                            ui.label(
                                                RichText::new(format!(
                                                    "ШАБЛОН · {scenario_count} групп"
                                                ))
                                                .strong()
                                                .size(8.0)
                                                .color(PURPLE),
                                            );
                                        } else {
                                            ui.label(
                                                RichText::new(format!("{step_count} шагов"))
                                                    .size(8.0)
                                                    .color(MUTED),
                                            );
                                        }
                                    });
                                })
                                .response
                                .interact(Sense::click());
                            if response.clicked() {
                                self.select_task(index);
                            }
                            ui.add_space(7.0);
                        }
                    });
            });
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
                    .is_some_and(|resolved| {
                        resolved
                            .steps
                            .iter()
                            .any(|step| matches!(step.action, Action::GitClone { .. }))
                    });
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
                                    "Можно заменить первый git-шаг одним или несколькими репозиториями из вашего GitHub.",
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
                let node_count = if task.is_template() {
                    groups.len()
                } else {
                    resolved.steps.len()
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
                        let width =
                            (node_count as f32 * node_stride + 180.0).max(ui.available_width());
                        let height = 690.0_f32.max(ui.available_height());
                        let (response, painter) =
                            ui.allocate_painter(Vec2::new(width, height), Sense::drag());
                        let bounds = response.rect;
                        paint_grid(&painter, bounds, self.dark);

                        let positions = (0..node_count)
                            .map(|index| {
                                let x = bounds.left() + 80.0 + index as f32 * node_stride;
                                let y = bounds.top() + 250.0 + ((index as f32 * 1.15).sin() * 78.0);
                                Pos2::new(x, y)
                            })
                            .collect::<Vec<_>>();

                        paint_connectors(&painter, &positions, node_size);

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
                            for (index, (step, position)) in
                                resolved.steps.iter().zip(positions.iter()).enumerate()
                            {
                                let rect = Rect::from_min_size(*position, node_size);
                                let selected = self.selected_step == Some(index);
                                let interaction = ui.interact(
                                    rect,
                                    Id::new(("scenario-step", task.id.as_str(), index)),
                                    Sense::click(),
                                );
                                if interaction.clicked() {
                                    self.selected_step = Some(index);
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
        let source_requires_git_auth = self
            .task_pack
            .as_ref()
            .and_then(|pack| {
                self.selected_task()
                    .and_then(|task| pack.resolve(&task.id).ok())
            })
            .and_then(|task| {
                task.steps
                    .into_iter()
                    .find(|step| matches!(step.action, Action::GitClone { .. }))
            })
            .is_some_and(|step| matches!(step.auth, AuthPolicy::GitCredential));
        let selection_requires_git_auth = source_requires_git_auth
            || matches!(self.github_picker.protocol, GithubCloneProtocol::Ssh)
            || self.github_picker.repositories.iter().any(|repository| {
                repository.is_private && self.github_picker.selected_ids.contains(&repository.id)
            });
        let mut configuration_changed = false;
        let mut request_refresh = false;
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
                                "Список загружается через вашу существующую сессию GitHub CLI; токен не попадает в ppduster.",
                            )
                            .size(9.0)
                            .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !self.github_picker.loading,
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
                        if self.github_picker.loading {
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
                            let before = self.github_picker.protocol;
                            ui.selectable_value(
                                &mut self.github_picker.protocol,
                                GithubCloneProtocol::Https,
                                "HTTPS",
                            );
                            ui.selectable_value(
                                &mut self.github_picker.protocol,
                                GithubCloneProtocol::Ssh,
                                "SSH",
                            );
                            configuration_changed |= before != self.github_picker.protocol;
                        });
                        ui.label(
                            RichText::new(
                                "Путь: <корень>/<owner>/<repository>; синхронизируется main. Протокол применяется при новом clone; существующий origin сохраняется.",
                            )
                            .size(8.0)
                            .color(MUTED),
                        );
                    });

                if selection_requires_git_auth {
                    ui.add_space(8.0);
                    Frame::new()
                        .fill(translucent(ORANGE, if self.dark { 34 } else { 15 }))
                        .stroke(Stroke::new(1.0, translucent(ORANGE, 90)))
                        .corner_radius(9)
                        .inner_margin(Margin::same(9))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(if matches!(
                                    self.github_picker.protocol,
                                    GithubCloneProtocol::Ssh
                                ) {
                                    "Для SSH нужен настроенный GitHub-ключ и доступный SSH agent. Загрузка списка через gh этого не проверяет."
                                } else {
                                    "Для private HTTPS обычный git должен использовать GitHub CLI как credential helper."
                                })
                                .size(9.0)
                                .color(ORANGE),
                            );
                            let (label, command) = if matches!(
                                self.github_picker.protocol,
                                GithubCloneProtocol::Ssh
                            ) {
                                ("Скопировать проверку SSH", "ssh -T git@github.com")
                            } else {
                                (
                                    "Скопировать настройку Git",
                                    "gh auth setup-git --hostname github.com",
                                )
                            };
                            if ui.button(label).clicked() {
                                ui.ctx().copy_text(command.into());
                            }
                        });
                }

                if let Some(error) = &self.github_picker.error {
                    ui.add_space(10.0);
                    error_box(ui, error, self.dark);
                    if ui.button("Скопировать команду входа").clicked() {
                        ui.ctx().copy_text("gh auth login --hostname github.com".into());
                    }
                }

                ui.add_space(10.0);
                if self.github_picker.loading && self.github_picker.repositories.is_empty() {
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
                                "Нужны установленный gh и выполненный gh auth login."
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

fn materialize_github_repositories(
    mut task: Task,
    repositories: &[GithubRepository],
    selected_ids: &BTreeSet<String>,
    destination_root: &str,
    protocol: GithubCloneProtocol,
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

    let source_index = task
        .steps
        .iter()
        .position(|step| matches!(step.action, Action::GitClone { .. }))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "сценарий {} не содержит git-шаг, который можно настроить",
                task.id
            )
        })?;
    let source_step = task.steps[source_index].clone();
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
        selected.push(repository);
    }
    selected.sort_by(|left, right| {
        left.name_with_owner
            .to_lowercase()
            .cmp(&right.name_with_owner.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut generated_steps = Vec::with_capacity(selected.len());
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

        let mut step = source_step.clone();
        step.id = format!(
            "{}/{}",
            source_step.id,
            github_step_slug(&repository.name_with_owner)
        );
        step.name = format!("Clone or update {}", repository.name_with_owner);
        step.check = None;
        // The desktop picker performs its own noninteractive credential gate.
        // Avoid the runner's terminal prompt in the background UI worker.
        step.auth = AuthPolicy::None;
        step.action = Action::GitClone {
            repo: github_clone_url(repository, protocol),
            dest: PathBuf::from(destination_root)
                .join(&repository.owner)
                .join(&repository.name)
                .display()
                .to_string(),
            branch: Some(branch.to_owned()),
        };
        generated_steps.push(step);
    }

    task.steps
        .splice(source_index..=source_index, generated_steps);
    task.validate().map_err(anyhow::Error::msg)?;
    Ok(task)
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

fn github_clone_url(repository: &GithubRepository, protocol: GithubCloneProtocol) -> String {
    match protocol {
        GithubCloneProtocol::Https => {
            format!("https://github.com/{}.git", repository.name_with_owner)
        }
        GithubCloneProtocol::Ssh => {
            format!("git@github.com:{}.git", repository.name_with_owner)
        }
    }
}

fn github_step_slug(name_with_owner: &str) -> String {
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
    slug.trim_matches('-').to_owned()
}

fn task_matches_query(task: &Task, normalized_query: &str) -> bool {
    normalized_query.is_empty()
        || task.name.to_lowercase().contains(normalized_query)
        || task.id.to_lowercase().contains(normalized_query)
        || task.description.to_lowercase().contains(normalized_query)
        || task
            .scenarios
            .iter()
            .any(|scenario| scenario.to_lowercase().contains(normalized_query))
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
                .find(|step| matches!(step.action, Action::GitClone { .. }))
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

fn paint_connectors(painter: &egui::Painter, positions: &[Pos2], node_size: Vec2) {
    for pair in positions.windows(2) {
        let from = pair[0] + Vec2::new(node_size.x, node_size.y * 0.5);
        let to = pair[1] + Vec2::new(0.0, node_size.y * 0.5);
        let bend = ((to.x - from.x) * 0.46).max(34.0);
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [
                from,
                from + Vec2::new(bend, 0.0),
                to - Vec2::new(bend, 0.0),
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
        Action::CreateDirectory(_) | Action::InspectPath(_) | Action::WriteFile(_) => CYAN,
        Action::CopyPath(_) | Action::DownloadFile { .. } | Action::GitClone { .. } => PURPLE,
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
        Action::CreateDirectory(_) => "+DIR",
        Action::InspectPath(_) => "INFO",
        Action::CopyPath(_) => "COPY",
        Action::WriteFile(_) => "TXT",
        Action::RemovePath(_) => "DEL",
        Action::GitClone { .. } => "⌘",
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
        Action::CreateDirectory(_) => "Папка",
        Action::InspectPath(_) => "Метаданные",
        Action::CopyPath(_) => "Копирование",
        Action::WriteFile(_) => "Запись файла",
        Action::RemovePath(_) => "Корзина",
        Action::GitClone { .. } => "Источник",
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
    if picker.selected_ids.is_empty() {
        return true;
    }
    if matches!(picker.protocol, GithubCloneProtocol::Ssh) {
        return git_clone_auth_ready("git@github.com:owner/repository.git");
    }
    if picker
        .repositories
        .iter()
        .any(|repository| repository.is_private && picker.selected_ids.contains(&repository.id))
    {
        return git_clone_auth_ready("https://github.com/owner/repository.git");
    }
    true
}

fn task_has_unready_git_credentials(task: &Task) -> bool {
    task.steps.iter().any(|step| {
        matches!(step.auth, AuthPolicy::GitCredential)
            && matches!(
                &step.action,
                Action::GitClone { repo, .. } if !git_clone_auth_ready(repo)
            )
    })
}

fn action_supports_gui_run(action: &Action) -> bool {
    match action {
        Action::ActivateLicense(_)
        | Action::AppStoreInstall(_)
        | Action::RunScript { .. }
        | Action::ConfigurePackageRegistryFiles { .. } => false,
        Action::CreateDirectory(_)
        | Action::InspectPath(_)
        | Action::CopyPath(_)
        | Action::WriteFile(_)
        | Action::RemovePath(_)
        | Action::GitClone { .. }
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
    TaskPack::load_many(&sources, false)
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
    fn search_matches_description_and_nested_scenario_ids() {
        let pack = load_tasks().unwrap();
        let template = pack.get("macos-developer-workstation").unwrap();

        assert!(task_matches_query(template, "deliberate order"));
        assert!(task_matches_query(template, &template.scenarios[0]));
        assert!(!task_matches_query(template, "definitely-not-a-scenario"));
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
    fn github_selection_expands_to_one_typed_git_step_per_repository() {
        let pack = load_tasks().unwrap();
        let task = pack.resolve("dev-brew-bootstrap").unwrap();
        let repositories = vec![
            github_repository("R2", "zeta/api", "trunk"),
            github_repository("R1", "acme/api", "main"),
        ];
        let selected_ids = BTreeSet::from(["R2".to_owned(), "R1".to_owned()]);

        let configured = materialize_github_repositories(
            task,
            &repositories,
            &selected_ids,
            "/tmp/workspaces",
            GithubCloneProtocol::Ssh,
        )
        .unwrap();

        assert_eq!(configured.steps.len(), 3);
        assert_eq!(configured.steps[0].id, "clone-repo/acme-api");
        assert_eq!(configured.steps[1].id, "clone-repo/zeta-api");
        assert!(configured.steps[..2]
            .iter()
            .all(|step| matches!(step.auth, AuthPolicy::None)));
        assert!(matches!(
            &configured.steps[0].action,
            Action::GitClone { repo, dest, branch }
                if repo == "git@github.com:acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch.as_deref() == Some("main")
        ));
        assert!(matches!(
            &configured.steps[1].action,
            Action::GitClone { repo, dest, branch }
                if repo == "git@github.com:zeta/api.git"
                    && dest == "/tmp/workspaces/zeta/api"
                    && branch.as_deref() == Some("main")
        ));
        assert!(matches!(
            configured.steps[2].action,
            Action::BrewInstall { .. }
        ));

        let report = run_task(&configured, &RunOptions::default()).unwrap();
        assert_eq!(report.steps.len(), 3);
        assert!(report.steps[0].summary.contains("acme/api"));
        assert!(report.steps[1].summary.contains("zeta/api"));
    }

    #[test]
    fn github_selection_rejects_missing_branch_and_path_traversal() {
        let pack = load_tasks().unwrap();
        let task = pack.resolve("dev-brew-bootstrap").unwrap();
        let mut missing_branch = github_repository("R1", "acme/empty", "main");
        missing_branch.main_branch = None;
        let selected_ids = BTreeSet::from(["R1".to_owned()]);
        let error = materialize_github_repositories(
            task.clone(),
            &[missing_branch],
            &selected_ids,
            "/tmp/workspaces",
            GithubCloneProtocol::Https,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ветки main"));

        let traversal = github_repository("R1", "../escape", "main");
        let error = materialize_github_repositories(
            task,
            &[traversal],
            &selected_ids,
            "/tmp/workspaces",
            GithubCloneProtocol::Https,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("недопустимое имя"));
    }

    #[test]
    fn github_selection_can_configure_the_first_git_step_in_a_template() {
        let pack = load_tasks().unwrap();
        let task = pack.resolve("macos-developer-workstation").unwrap();
        let original_steps = task.steps.len();
        let repositories = [
            github_repository("R1", "acme/workstation", "trunk"),
            github_repository("R2", "zeta/workstation", "main"),
        ];
        let selected_ids = BTreeSet::from(["R1".to_owned(), "R2".to_owned()]);

        let configured = materialize_github_repositories(
            task,
            &repositories,
            &selected_ids,
            "/tmp/workspaces",
            GithubCloneProtocol::Https,
        )
        .unwrap();

        assert_eq!(configured.steps.len(), original_steps + 1);
        let selected_step = configured
            .steps
            .iter()
            .find(|step| step.id.contains("acme-workstation"))
            .unwrap();
        assert!(matches!(
            &selected_step.action,
            Action::GitClone { branch, .. } if branch.as_deref() == Some("main")
        ));

        let template = pack.get("macos-developer-workstation").unwrap();
        let groups =
            scenario_groups(&pack, template, &RunOptions::default(), Some(&configured)).unwrap();
        assert_eq!(
            groups.iter().map(|group| group.step_count).sum::<usize>(),
            configured.steps.len()
        );
        let configured_group = groups
            .iter()
            .find(|group| {
                group
                    .step_summaries
                    .iter()
                    .any(|summary| summary.contains("acme/workstation"))
            })
            .unwrap();
        assert!(configured_group
            .step_summaries
            .iter()
            .any(|summary| summary.contains("zeta/workstation")));
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
