use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::PackTrust;
use ppduster::automation::{
    run_task, Action, ReleaseChannel, RunOptions, RunReport, Step, StepStatus, Task, TaskPack,
    TaskSource,
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
    tasks: Vec<Task>,
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
        let (tasks, load_error) = match load_tasks() {
            Ok(pack) => (pack.tasks, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };
        let selected_task = tasks
            .iter()
            .position(|task| task.id == "bambu-studio-install")
            .unwrap_or(0);
        Self {
            tasks,
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
        self.tasks.get(self.selected_task)
    }

    fn build_plan(&mut self) {
        let Some(task) = self.selected_task().cloned() else {
            return;
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
        let Some(task) = self.selected_task().cloned() else {
            return;
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
        let mut command = format!("ppduster setup run {}", task.id);
        if task
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
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {} шагов",
                                        task.id,
                                        task.steps.len()
                                    ))
                                    .size(9.0)
                                    .color(MUTED),
                                );
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

                let query = self.search.trim().to_ascii_lowercase();
                let visible = self
                    .tasks
                    .iter()
                    .enumerate()
                    .filter(|(_, task)| {
                        query.is_empty()
                            || task.name.to_ascii_lowercase().contains(&query)
                            || task.id.to_ascii_lowercase().contains(&query)
                    })
                    .map(|(index, task)| {
                        (
                            index,
                            task.name.clone(),
                            task.id.clone(),
                            task.steps.len(),
                            task.platform.as_str().to_string(),
                        )
                    })
                    .collect::<Vec<_>>();

                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(8.0);
                        for (index, name, id, step_count, platform) in visible {
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
                                    ui.label(
                                        RichText::new(platform.to_uppercase())
                                            .size(8.0)
                                            .color(CYAN),
                                    );
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
            .exact_size(330.0)
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
                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.label(
                        RichText::new(&task.name)
                            .strong()
                            .size(18.0)
                            .color(text(self.dark)),
                    );
                    ui.add_space(5.0);
                    ui.label(RichText::new(&task.description).size(10.0).color(MUTED));
                    ui.add_space(14.0);

                    if task
                        .steps
                        .iter()
                        .any(|step| matches!(step.action, Action::BambuStudioRelease(_)))
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
                        .add_sized(
                            [ui.available_width(), 36.0],
                            egui::Button::new(
                                RichText::new("Проверить план")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
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
                        && task_supports_gui_run(&task);
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
                    if !task_supports_gui_run(&task) {
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
                    section_label(ui, "ВЫБРАННЫЙ ШАГ");
                    if let Some(step_index) = self.selected_step {
                        if let Some(step) = task.steps.get(step_index) {
                            ui.label(
                                RichText::new(step_title(step))
                                    .strong()
                                    .size(14.0)
                                    .color(text(self.dark)),
                            );
                            ui.label(
                                RichText::new(step.id.clone())
                                    .monospace()
                                    .size(9.0)
                                    .color(MUTED),
                            );
                            ui.add_space(8.0);
                            let yaml = serde_yaml::to_string(step)
                                .unwrap_or_else(|error| format!("Ошибка: {error}"));
                            Frame::new()
                                .fill(code_surface(self.dark))
                                .corner_radius(8)
                                .inner_margin(Margin::same(9))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(yaml)
                                            .monospace()
                                            .size(9.0)
                                            .color(text(self.dark)),
                                    );
                                });
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
                ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let width =
                            (task.steps.len() as f32 * 286.0 + 180.0).max(ui.available_width());
                        let height = 690.0_f32.max(ui.available_height());
                        let (response, painter) =
                            ui.allocate_painter(Vec2::new(width, height), Sense::drag());
                        let bounds = response.rect;
                        paint_grid(&painter, bounds, self.dark);

                        let node_size = Vec2::new(232.0, 116.0);
                        let positions = task
                            .steps
                            .iter()
                            .enumerate()
                            .map(|(index, _)| {
                                let x = bounds.left() + 80.0 + index as f32 * 286.0;
                                let y = bounds.top() + 250.0 + ((index as f32 * 1.15).sin() * 78.0);
                                Pos2::new(x, y)
                            })
                            .collect::<Vec<_>>();

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
                            painter.circle_stroke(
                                from,
                                11.0,
                                Stroke::new(2.0, translucent(PURPLE, 80)),
                            );
                            painter.circle_stroke(
                                to,
                                11.0,
                                Stroke::new(2.0, translucent(PURPLE, 80)),
                            );
                        }

                        for (index, (step, position)) in
                            task.steps.iter().zip(positions.iter()).enumerate()
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

                        painter.text(
                            Pos2::new(bounds.left() + 80.0, bounds.top() + 92.0),
                            Align2::LEFT_TOP,
                            "СЦЕНАРИЙ",
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
    let (status_text, status_color) = match status {
        Some(StepStatus::Satisfied) => ("ГОТОВО", CYAN),
        Some(StepStatus::Failed) => ("ОШИБКА", Color32::from_rgb(194, 64, 64)),
        Some(StepStatus::Applied) => ("ВЫПОЛНЕНО", CYAN),
        Some(StepStatus::Skipped) => ("ПРОПУЩЕНО", MUTED),
        _ => ("ОЖИДАЕТ", PURPLE),
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
                Action::ActivateLicense(_) | Action::AppStoreInstall(_)
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
    }

    #[test]
    fn gui_execution_excludes_vendor_ui_flows() {
        let pack = load_tasks().unwrap();
        assert!(task_supports_gui_run(
            pack.get("bambu-studio-install").unwrap()
        ));
        assert!(!task_supports_gui_run(
            pack.get("lightburn-install-activate").unwrap()
        ));
        assert!(task_supports_gui_run(
            pack.get("app-store-bootstrap").unwrap()
        ));
    }
}
