use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::PackTrust;
use ppduster::automation::{
    describe_step, run_task, Action, ReleaseChannel, RunOptions, RunReport, Step, StepStatus, Task,
    TaskPack, TaskSource,
};
use std::path::PathBuf;
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
    plan_error: Option<String>,
    dark: bool,
    confirm_run: bool,
    running: bool,
    run_receiver: Option<Receiver<Result<RunReport, String>>>,
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
            plan_error: None,
            dark: false,
            confirm_run: false,
            running: false,
            run_receiver: None,
        }
    }

    fn selected_task(&self) -> Option<&Task> {
        self.task_pack.as_ref()?.tasks.get(self.selected_task)
    }

    fn resolved_selected_task(&self) -> anyhow::Result<Task> {
        let task = self
            .selected_task()
            .ok_or_else(|| anyhow::anyhow!("сценарий не выбран"))?;
        self.task_pack
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("библиотека сценариев не загружена"))?
            .resolve(&task.id)
    }

    fn build_plan(&mut self) {
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
        self.report = None;
        self.plan_error = None;
    }

    fn command_for_selected(&self) -> Option<String> {
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
        self.top_bar(ui);
        self.left_library(ui);
        self.right_inspector(ui);
        self.canvas(ui);
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
                    .map(|(pack, options)| scenario_groups(pack, &task, options))
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

                    if resolved_task.as_ref().is_some_and(|resolved| {
                        resolved
                            .steps
                            .iter()
                            .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
                    })
                    {
                        section_label(ui, "КАНАЛ РЕЛИЗА");
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
                    ui.checkbox(&mut self.allow_elevation, "Разрешить elevation");
                    ui.checkbox(&mut self.allow_shell, "Разрешить shell");
                    ui.label(
                        RichText::new("Без этих флагов опасные шаги не попадут в план.")
                            .size(9.0)
                            .color(MUTED),
                    );
                    ui.add_space(14.0);

                    if ui
                        .add_enabled(
                            resolved_task.is_some(),
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
                    let can_run = self.report.is_some()
                        && self.plan_error.is_none()
                        && !self.running
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
                        ui.label(
                            RichText::new(
                                "Этот сценарий требует терминала или vendor UI; используйте команду ниже.",
                            )
                            .size(9.0)
                            .color(MUTED),
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
                    }

                    if let Some(error) = &self.plan_error {
                        ui.add_space(12.0);
                        error_box(ui, error, self.dark);
                    }
                    if let Some(report) = &self.report {
                        ui.add_space(12.0);
                        Frame::new()
                            .fill(translucent(CYAN, if self.dark { 35 } else { 15 }))
                            .stroke(Stroke::new(1.0, translucent(CYAN, 90)))
                            .corner_radius(10)
                            .inner_margin(Margin::same(10))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "План готов · {} шагов",
                                        report.steps.len()
                                    ))
                                    .strong()
                                    .color(CYAN),
                                );
                                ui.label(
                                    RichText::new("Никакие изменения не применены.")
                                        .size(9.0)
                                        .color(MUTED),
                                );
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
                        .map(|pack| scenario_groups(pack, &task, &options))
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
) -> anyhow::Result<Vec<ScenarioGroup>> {
    template
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
        .collect()
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
        Action::DownloadFile { .. } | Action::GitClone { .. } => PURPLE,
        Action::ExtractArchive { .. } | Action::InstallDmg { .. } | Action::InstallPkg { .. } => {
            ORANGE
        }
        Action::MacosRequirements { .. } => CYAN,
        Action::BrewInstall { .. } | Action::AppStoreInstall(_) => BLUE,
        Action::RunCommand { .. } => Color32::from_rgb(139, 95, 191),
        Action::BambuStudioRelease(_) => ORANGE,
        Action::ActivateLicense(_) => Color32::from_rgb(183, 90, 115),
        Action::ConfigurePackageRegistryFiles { .. } => CYAN,
    }
}

fn action_icon(action: &Action) -> &'static str {
    match action {
        Action::GitClone { .. } => "⌘",
        Action::BrewInstall { .. } => "B",
        Action::RunCommand { .. } => ">_",
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
        Action::GitClone { .. } => "Источник",
        Action::BrewInstall { .. } | Action::AppStoreInstall(_) => "Пакет",
        Action::RunCommand { .. } => "Команда",
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
    task.steps.iter().all(|step| {
        matches!(step.auth, ppduster::automation::AuthPolicy::None)
            && !matches!(
                step.action,
                Action::ActivateLicense(_)
                    | Action::AppStoreInstall(_)
                    | Action::ConfigurePackageRegistryFiles { .. }
            )
    })
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
            &pack.resolve("dev-dodopizza-package-registries").unwrap()
        ));
    }

    #[test]
    fn template_canvas_uses_direct_scenario_groups() {
        let pack = load_tasks().unwrap();
        let template = pack.get("macos-developer-workstation").unwrap();
        assert!(template.is_template());

        let resolved = pack.resolve(&template.id).unwrap();
        let groups = scenario_groups(&pack, template, &RunOptions::default()).unwrap();

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
}
