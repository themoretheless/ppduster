use anyhow::Context;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::binding::validate_literal_binding;
use ppduster::automation::block::default_step;
use ppduster::automation::PackTrust;
use ppduster::automation::{
    block_definition, definition_for_action, describe_step, first_scenario_path, load_project_yaml,
    make_project_external, project_group_entries, project_group_entries_mut, run_task,
    validate_project as validate_project_structure, Action, ActionKind, ActionNode, AuthPolicy,
    Binding, BlockPolicyCapabilities, CanvasPoint, CanvasView, ComparisonOperator, ComposerCanvas,
    ContextPathSegment, ContextScope, EdgePort, ElevationPolicy, ExpressionLimits, ExpressionV1,
    ExpressionValue, FieldRef, ForEachNode, GraphEdge, GraphNode, GraphValidationError,
    GraphValidationErrorKind, IfNode, IndeterminatePolicy, JoinMode, JoinNode, LoopFailurePolicy,
    ObjectSchema, PolicyRequirement, ProjectEntry, ReferenceV1, ReleaseChannel, RuleOutcomePolicy,
    RunOptions, RunReport, ScenarioProject, ScenarioProjectFile, ScriptInterpreter, SemanticFormat,
    Sensitivity, Step, StepCondition, StepStatus, SwitchCase, SwitchNode, Task, TaskFile, TaskPack,
    TaskSource, TemplatePart, TrustRequirement, WorkflowGraph,
};
#[cfg(test)]
use ppduster::automation::{
    ContextStore, CopyPathAction, CreateDirectoryAction, InspectPathAction, RemovePathAction,
    StepLogEntry, StepOutput, StepReport, StructuredStepOutput, WriteConflictPolicy,
    WriteFileAction,
};
use ppduster::automation::{ContextType, FieldSchema};
use ppduster::github::{list_accessible_repositories, login_via_web, GithubRepository};
use regex::RegexBuilder;
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

const COMPACT_VIEWPORT_WIDTH: f32 = 980.0;
const WIDE_VIEWPORT_WIDTH: f32 = 1440.0;
const COMPACT_LIBRARY_WIDTH: f32 = 220.0;
const WIDE_LIBRARY_WIDTH: f32 = 270.0;
const COMPACT_INSPECTOR_WIDTH: f32 = 340.0;
const WIDE_INSPECTOR_WIDTH: f32 = 360.0;
const CANVAS_MIN_ZOOM: f32 = 0.35;
const CANVAS_MAX_ZOOM: f32 = 2.5;
const CANVAS_ZOOM_STEP: f32 = 1.2;
const CANVAS_FIT_PADDING: f32 = 56.0;

/// Keep the editor usable down to the minimum supported viewport while
/// preserving the roomier desktop proportions on wide windows.
fn workspace_panel_widths(viewport_width: f32) -> (f32, f32) {
    let wide_fraction = ((viewport_width - COMPACT_VIEWPORT_WIDTH)
        / (WIDE_VIEWPORT_WIDTH - COMPACT_VIEWPORT_WIDTH))
        .clamp(0.0, 1.0);
    (
        egui::lerp(COMPACT_LIBRARY_WIDTH..=WIDE_LIBRARY_WIDTH, wide_fraction),
        egui::lerp(
            COMPACT_INSPECTOR_WIDTH..=WIDE_INSPECTOR_WIDTH,
            wide_fraction,
        ),
    )
}

/// Keep inspector widgets inside the fixed right panel even when schemas,
/// bindings, paths, or YAML contain very long unbroken strings.
fn bounded_inspector_scroll<R>(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::containers::scroll_area::ScrollAreaOutput<R> {
    let outer_width = ui.available_width().max(0.0);
    ScrollArea::vertical()
        .id_salt(id_salt)
        .max_width(outer_width)
        .horizontal_scroll_offset(0.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let content_width = ui.available_width().max(0.0);
            ui.set_width(content_width);
            // Prose explicitly opts into wrapping. Compact labels, buttons,
            // combo selections and popup rows truncate instead of widening
            // the parent UI beyond the inspector.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            add_contents(ui)
        })
}

#[derive(Debug, Clone)]
struct ScenarioGroup {
    id: String,
    name: String,
    description: String,
    step_count: usize,
    step_summaries: Vec<String>,
}

const fn default_canvas_zoom() -> f32 {
    1.0
}

trait CanvasViewExt: Sized {
    fn sanitized(self) -> Self;
    fn transform(self, viewport: Rect) -> egui::emath::TSTransform;
    #[cfg(test)]
    fn world_to_screen(self, viewport: Rect, point: Pos2) -> Pos2;
    fn screen_to_world(self, viewport: Rect, point: Pos2) -> Pos2;
    #[cfg(test)]
    fn screen_rect(self, viewport: Rect, rect: Rect) -> Rect;
    fn visible_world_rect(self, viewport: Rect) -> Rect;
    fn from_visible_world_rect(world: Rect, viewport: Rect) -> Self;
    #[cfg(test)]
    fn pan_by(&mut self, screen_delta: Vec2);
    fn zoom_about(&mut self, viewport: Rect, anchor: Pos2, factor: f32);
    fn fit(world_bounds: Rect, viewport: Rect, padding: f32) -> Self;
}

impl CanvasViewExt for CanvasView {
    fn sanitized(mut self) -> Self {
        if !self.pan.x.is_finite() {
            self.pan.x = 0.0;
        }
        if !self.pan.y.is_finite() {
            self.pan.y = 0.0;
        }
        if !self.zoom.is_finite() {
            self.zoom = default_canvas_zoom();
        }
        self.zoom = self.zoom.clamp(CANVAS_MIN_ZOOM, CANVAS_MAX_ZOOM);
        self
    }

    fn transform(self, viewport: Rect) -> egui::emath::TSTransform {
        let view = self.sanitized();
        egui::emath::TSTransform::new(
            viewport.min.to_vec2() + Vec2::new(view.pan.x, view.pan.y),
            view.zoom,
        )
    }

    #[cfg(test)]
    fn world_to_screen(self, viewport: Rect, point: Pos2) -> Pos2 {
        self.transform(viewport) * point
    }

    fn screen_to_world(self, viewport: Rect, point: Pos2) -> Pos2 {
        self.transform(viewport).inverse() * point
    }

    #[cfg(test)]
    fn screen_rect(self, viewport: Rect, rect: Rect) -> Rect {
        self.transform(viewport) * rect
    }

    fn visible_world_rect(self, viewport: Rect) -> Rect {
        self.transform(viewport).inverse() * viewport
    }

    fn from_visible_world_rect(world: Rect, viewport: Rect) -> Self {
        if !world.is_positive() || !viewport.is_positive() {
            return Self::default();
        }
        let zoom = (viewport.width() / world.width())
            .min(viewport.height() / world.height())
            .clamp(CANVAS_MIN_ZOOM, CANVAS_MAX_ZOOM);
        Self {
            pan: CanvasPoint {
                x: -world.left() * zoom,
                y: -world.top() * zoom,
            },
            zoom,
        }
        .sanitized()
    }

    #[cfg(test)]
    fn pan_by(&mut self, screen_delta: Vec2) {
        if screen_delta.is_finite() {
            self.pan.x += screen_delta.x;
            self.pan.y += screen_delta.y;
        }
        *self = self.sanitized();
    }

    fn zoom_about(&mut self, viewport: Rect, anchor: Pos2, factor: f32) {
        *self = self.sanitized();
        if !factor.is_finite() || factor <= 0.0 || !viewport.is_positive() {
            return;
        }
        let anchor_world = self.screen_to_world(viewport, anchor);
        let next_zoom = (self.zoom * factor).clamp(CANVAS_MIN_ZOOM, CANVAS_MAX_ZOOM);
        self.zoom = next_zoom;
        self.pan.x = anchor.x - viewport.left() - anchor_world.x * next_zoom;
        self.pan.y = anchor.y - viewport.top() - anchor_world.y * next_zoom;
        *self = self.sanitized();
    }

    fn fit(world_bounds: Rect, viewport: Rect, padding: f32) -> Self {
        if !world_bounds.is_positive() || !viewport.is_positive() {
            return Self::default();
        }
        let available =
            (viewport.size() - Vec2::splat(padding.max(0.0) * 2.0)).max(Vec2::splat(1.0));
        let zoom = (available.x / world_bounds.width())
            .min(available.y / world_bounds.height())
            .clamp(CANVAS_MIN_ZOOM, CANVAS_MAX_ZOOM);
        let pan = viewport.center() - viewport.min - world_bounds.center().to_vec2() * zoom;
        Self {
            pan: CanvasPoint { x: pan.x, y: pan.y },
            zoom,
        }
        .sanitized()
    }
}

fn graph_canvas_world_bounds(canvas: &ComposerCanvas, node_size: Vec2) -> Rect {
    let header_bounds = Rect::from_min_size(Pos2::new(80.0, 88.0), Vec2::new(520.0, 80.0));
    let mut points = canvas.positions.values();
    let Some(first) = points.next() else {
        return header_bounds.union(Rect::from_min_size(Pos2::new(80.0, 250.0), node_size));
    };
    let mut min = Pos2::new(first.x, first.y);
    let mut max = min + node_size;
    for point in points {
        min.x = min.x.min(point.x);
        min.y = min.y.min(point.y);
        max.x = max.x.max(point.x + node_size.x);
        max.y = max.y.max(point.y + node_size.y);
    }
    header_bounds.union(Rect::from_min_max(min, max))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposerGraphAttach {
    RootStart,
    RootAfter {
        node_id: String,
    },
    NestedStart {
        scope: ComposerGraphNestedScope,
    },
    NestedAfter {
        scope: ComposerGraphNestedScope,
        node_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposerGraphNestedScope {
    ForEachBody { owner_id: String },
    IfThen { owner_id: String },
    IfElse { owner_id: String },
    SwitchCase { owner_id: String, case_id: String },
    SwitchDefault { owner_id: String },
}

impl ComposerGraphNestedScope {
    fn owner_id(&self) -> &str {
        match self {
            Self::ForEachBody { owner_id }
            | Self::IfThen { owner_id }
            | Self::IfElse { owner_id }
            | Self::SwitchCase { owner_id, .. }
            | Self::SwitchDefault { owner_id } => owner_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerGraphBlockKind {
    Action(ActionKind),
    ForEach,
    If,
    Switch,
    Join,
}

#[derive(Debug, Clone, Copy)]
#[cfg(test)]
enum ComposerBlockKind {
    GithubListRepositories,
    ForEach,
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

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ComposerArraySource {
    step_id: String,
    step_name: String,
    path: String,
    item: String,
    item_type: ContextType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposerBindableField {
    path: String,
    value_type: ContextType,
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ComposerIndexedBinding {
    source_step: String,
    array_path: String,
    index: usize,
    field_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ComposerLoopSource {
    step_id: String,
    step_name: String,
    source_step: String,
    array_path: String,
    item: String,
    fields: Vec<String>,
    item_type: ContextType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ComposerLoopBinding {
    loop_step: String,
    field_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
enum ComposerLoopDestinationSuffix {
    FullName {
        field_path: String,
    },
    OwnerName {
        owner_path: String,
        name_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
struct ComposerLoopDestinationBinding {
    root: String,
    suffix: ComposerLoopDestinationSuffix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposerConditionField {
    reference: FieldRef,
    label: String,
    value_type: ContextType,
    required: bool,
    nullable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerConditionOperator {
    Equal,
    NotEqual,
    Exists,
    IsNull,
    IsEmpty,
    Contains,
    StartsWith,
    EndsWith,
    Matches,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl ComposerConditionOperator {
    const fn label(self) -> &'static str {
        match self {
            Self::Equal => "Равно",
            Self::NotEqual => "Не равно",
            Self::Exists => "Существует",
            Self::IsNull => "Равно null",
            Self::IsEmpty => "Пусто",
            Self::Contains => "Содержит",
            Self::StartsWith => "Начинается с",
            Self::EndsWith => "Заканчивается на",
            Self::Matches => "Регулярное выражение",
            Self::LessThan => "Меньше",
            Self::LessThanOrEqual => "Меньше или равно",
            Self::GreaterThan => "Больше",
            Self::GreaterThanOrEqual => "Больше или равно",
        }
    }

    const fn requires_literal(self) -> bool {
        !matches!(self, Self::Exists | Self::IsNull | Self::IsEmpty)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerLiteralKind {
    Null,
    Bool,
    Integer,
    Number,
    String,
}

impl ComposerLiteralKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
        }
    }

    fn from_value(value: &ExpressionValue) -> Option<Self> {
        match value {
            ExpressionValue::Null => Some(Self::Null),
            ExpressionValue::Bool(_) => Some(Self::Bool),
            ExpressionValue::Int(_) | ExpressionValue::UInt(_) => Some(Self::Integer),
            ExpressionValue::Float(_) => Some(Self::Number),
            ExpressionValue::String(_) => Some(Self::String),
            ExpressionValue::List(_) | ExpressionValue::Object(_) => None,
        }
    }

    fn default_value(self) -> ExpressionValue {
        match self {
            Self::Null => ExpressionValue::Null,
            Self::Bool => ExpressionValue::Bool(false),
            Self::Integer => ExpressionValue::Int(0),
            Self::Number => ExpressionValue::Float(0.0),
            Self::String => ExpressionValue::String(String::new()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SimpleConditionRule {
    field: FieldRef,
    operator: ComposerConditionOperator,
    literal: Option<ExpressionValue>,
}

#[derive(Debug, Clone, PartialEq)]
enum ComposerConditionRule {
    Clause(SimpleConditionRule),
    All(Vec<ComposerConditionRule>),
    Any(Vec<ComposerConditionRule>),
    Not(Box<ComposerConditionRule>),
}

// The visual editor intentionally exposes a smaller budget than the runtime
// expression engine. This keeps a malformed or machine-generated AST from
// turning the inspector into an unbounded recursive widget tree. Expressions
// outside this envelope remain visible as YAML and are never rewritten.
const CONDITION_EDITOR_MAX_DEPTH: usize = 8;
const CONDITION_EDITOR_MAX_NODES: usize = 64;
#[cfg(test)]
#[allow(dead_code)]
const COMPOSER_MAX_ARRAY_ORDINAL: usize = 10_000;

#[cfg(test)]
fn composer_array_sources(task: &Task, before_index: usize) -> Vec<ComposerArraySource> {
    composer_array_sources_scoped(task, before_index, None)
}

#[cfg(test)]
fn composer_array_sources_scoped(
    task: &Task,
    before_index: usize,
    canvas: Option<&ComposerCanvas>,
) -> Vec<ComposerArraySource> {
    let mut sources = Vec::new();
    for (index, step) in task.steps.iter().enumerate().take(before_index) {
        if matches!(step.action, Action::ForEach { .. })
            || composer_step_is_loop_body_child(task, index, canvas)
        {
            continue;
        }
        let definition = definition_for_action(&step.action);
        let mut arrays = Vec::new();
        collect_schema_arrays(&definition.output_schema, "", &mut arrays);
        sources.extend(
            arrays
                .into_iter()
                .map(|(path, item_type)| ComposerArraySource {
                    step_id: step.id.clone(),
                    step_name: step_title(step),
                    item: item_alias_for_array_path(&path),
                    path,
                    item_type,
                }),
        );
    }
    sources
}

#[cfg(test)]
fn composer_loop_sources(task: &Task, before_index: usize) -> Vec<ComposerLoopSource> {
    composer_loop_sources_scoped(task, before_index, None)
}

#[cfg(test)]
fn composer_loop_sources_scoped(
    task: &Task,
    before_index: usize,
    canvas: Option<&ComposerCanvas>,
) -> Vec<ComposerLoopSource> {
    task.steps
        .iter()
        .enumerate()
        .take(before_index)
        .filter_map(|(index, step)| match &step.action {
            Action::ForEach {
                source_step,
                array_path,
                item,
                fields,
            } => {
                let item_type = composer_array_sources_scoped(task, index, canvas)
                    .into_iter()
                    .find(|source| source.step_id == *source_step && source.path == *array_path)
                    .map(|source| project_item_type(&source.item_type, fields))
                    .unwrap_or(ContextType::Any);
                Some(ComposerLoopSource {
                    step_id: step.id.clone(),
                    step_name: step_title(step),
                    source_step: source_step.clone(),
                    array_path: array_path.clone(),
                    item: item.clone(),
                    fields: fields.clone(),
                    item_type,
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
fn composer_parent_loop_id(
    task: &Task,
    index: usize,
    canvas: Option<&ComposerCanvas>,
) -> Option<String> {
    let step = task.steps.get(index)?;
    let parent = canvas?.parents.get(&step.id)?;
    let candidate = task.steps.get(index.checked_sub(1)?)?;
    (candidate.id == *parent && matches!(candidate.action, Action::ForEach { .. }))
        .then(|| candidate.id.clone())
}

#[cfg(test)]
fn composer_step_is_loop_body_child(
    task: &Task,
    index: usize,
    canvas: Option<&ComposerCanvas>,
) -> bool {
    composer_parent_loop_id(task, index, canvas).is_some()
}

#[cfg(test)]
fn composer_parent_accepts_new_child(task: &Task, parent_id: &str) -> bool {
    task.steps
        .iter()
        .position(|step| step.id == parent_id)
        .is_none_or(|index| {
            !matches!(task.steps[index].action, Action::ForEach { .. })
                || index + 1 == task.steps.len()
        })
}

fn composer_canvas_edge_is_visible(task: &Task, child_id: &str, parent_id: &str) -> bool {
    if parent_id == "start" {
        return true;
    }
    let Some(parent_index) = task.steps.iter().position(|step| step.id == parent_id) else {
        return false;
    };
    if !matches!(task.steps[parent_index].action, Action::ForEach { .. }) {
        return task.steps.iter().any(|step| step.id == child_id);
    }
    let Some(child_index) = task.steps.iter().position(|step| step.id == child_id) else {
        return false;
    };
    child_index == parent_index + 1
        && composer_step_has_loop_membership(&task.steps[child_index], parent_id)
}

fn binding_references_loop_item(binding: &Binding, loop_id: &str) -> bool {
    let references_loop = |field: &FieldRef| {
        matches!(
            &field.scope,
            ContextScope::LoopItem { step_id } if step_id == loop_id
        )
    };
    match binding {
        Binding::Field { field } => references_loop(field),
        Binding::Interpolated { parts } => parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Field { field } if references_loop(field))),
        Binding::Literal { .. } | Binding::Template { .. } => false,
    }
}

fn composer_step_has_loop_membership(step: &Step, loop_id: &str) -> bool {
    match &step.action {
        Action::ForEachGitCloneIfMissing { loop_step, .. } => loop_step == loop_id,
        Action::GitInspect { .. } => step
            .bindings
            .get("repo")
            .is_some_and(|binding| binding_references_loop_item(binding, loop_id)),
        _ => step
            .bindings
            .values()
            .any(|binding| binding_references_loop_item(binding, loop_id)),
    }
}

fn validate_composer_canvas(task: &Task, canvas: &ComposerCanvas) -> Result<(), String> {
    if let Some(graph) = &task.graph {
        validate_graph_for_ui(task, graph)?;
        let valid = graph_node_ids(graph);
        if let Some(stale) = canvas
            .positions
            .keys()
            .find(|id| id.as_str() != "start" && !valid.contains(id.as_str()))
        {
            return Err(format!(
                "канвас сценария {} содержит позицию неизвестного узла {stale}",
                task.id
            ));
        }
        return Ok(());
    }
    for (child_id, parent_id) in &canvas.parents {
        let Some(child_index) = task.steps.iter().position(|step| step.id == *child_id) else {
            return Err(format!(
                "канвас сценария {} ссылается на неизвестный дочерний блок {child_id}",
                task.id
            ));
        };
        if parent_id == "start" {
            continue;
        }
        let Some(parent_index) = task.steps.iter().position(|step| step.id == *parent_id) else {
            return Err(format!(
                "канвас сценария {} ссылается на неизвестный родительский блок {parent_id}",
                task.id
            ));
        };
        if !matches!(task.steps[parent_index].action, Action::ForEach { .. }) {
            continue;
        }
        if child_index != parent_index + 1 {
            return Err(format!(
                "блок {child_id} нарисован дочерним для For each {parent_id}, но не идёт сразу после него"
            ));
        }
        if !composer_step_has_loop_membership(&task.steps[child_index], parent_id) {
            return Err(format!(
                "дочерний блок {child_id} должен использовать структурный контекст текущего элемента For each {parent_id}"
            ));
        }
    }
    for loop_step in task
        .steps
        .iter()
        .filter(|step| matches!(step.action, Action::ForEach { .. }))
    {
        let children = canvas
            .parents
            .iter()
            .filter(|(_, parent)| *parent == &loop_step.id)
            .count();
        if children != 1 {
            return Err(format!(
                "For each {} должен иметь ровно один непосредственный дочерний блок, найдено: {children}",
                loop_step.id
            ));
        }
    }
    Ok(())
}

fn validate_project_for_editing(project: &ScenarioProject) -> Result<(), String> {
    // Deserialization already performs the one-way v1 -> v3 import. Editing
    // must never repair or mutate Task.steps as a shadow authoring model. A
    // graph may itself be invalid here: the inspector is the place where a
    // loaded draft is diagnosed and explicitly repaired.
    if project.id.trim().is_empty() || project.name.trim().is_empty() {
        return Err("У проекта должны быть заполнены ID и название.".into());
    }

    fn visit(entries: &[ProjectEntry], ids: &mut BTreeSet<String>) -> Result<(), String> {
        for entry in entries {
            match entry {
                ProjectEntry::Group { id, name, entries } => {
                    if id.trim().is_empty() || name.trim().is_empty() {
                        return Err("У каждой группы должны быть заполнены ID и название.".into());
                    }
                    visit(entries, ids)?;
                }
                ProjectEntry::Scenario { task } => {
                    if task.id.trim().is_empty()
                        || task.id.contains('/')
                        || task.name.trim().is_empty()
                        || task.description.trim().is_empty()
                    {
                        return Err(
                            "У каждого сценария должны быть корректные ID, название и описание."
                                .into(),
                        );
                    }
                    if task.graph.is_none() || !task.steps.is_empty() || !task.scenarios.is_empty()
                    {
                        return Err(format!(
                            "Сценарий «{}» должен быть импортирован в WorkflowGraph v3 перед редактированием.",
                            task.name
                        ));
                    }
                    if !ids.insert(task.id.clone()) {
                        return Err(format!("Повторяется ID сценария «{}».", task.id));
                    }
                }
            }
        }
        Ok(())
    }

    visit(&project.entries, &mut BTreeSet::new())
}

#[cfg(test)]
fn composer_bind_git_inspect_to_parent_loop(task: &Task, parent: &str, child: &mut Step) -> bool {
    if !matches!(child.action, Action::GitInspect { .. })
        || !task
            .steps
            .last()
            .is_some_and(|step| step.id == parent && matches!(step.action, Action::ForEach { .. }))
    {
        return false;
    }
    let Some(loop_source) = composer_loop_sources(task, task.steps.len())
        .into_iter()
        .find(|source| source.step_id == parent)
    else {
        return false;
    };
    let input_schema = definition_for_action(&child.action).input_schema;
    let Some(expected) = input_schema.field("repo") else {
        return false;
    };
    if !composer_insert_default_loop_binding(&mut child.bindings, "repo", &loop_source, expected) {
        return false;
    }
    if let Some(suffix) = composer_loop_destination_suffixes(&loop_source)
        .into_iter()
        .next()
    {
        child.bindings.insert(
            "dest".into(),
            composer_loop_destination_binding(&loop_source, "$HOME/Developer", &suffix),
        );
    }
    true
}

#[cfg(test)]
fn composer_condition_fields_scoped(
    task: &Task,
    before_index: usize,
    canvas: Option<&ComposerCanvas>,
) -> Vec<ComposerConditionField> {
    let mut fields = Vec::new();
    for (index, step) in task.steps.iter().enumerate().take(before_index) {
        if matches!(step.action, Action::ForEach { .. })
            || composer_step_is_loop_body_child(task, index, canvas)
        {
            continue;
        }
        let definition = definition_for_action(&step.action);
        collect_condition_fields(
            &step.id,
            &definition.output_schema,
            "",
            true,
            false,
            Sensitivity::Public,
            &mut fields,
        );
    }
    fields
}

#[cfg(test)]
fn collect_condition_fields(
    step_id: &str,
    schema: &ObjectSchema,
    prefix: &str,
    inherited_required: bool,
    inherited_nullable: bool,
    inherited_sensitivity: Sensitivity,
    output: &mut Vec<ComposerConditionField>,
) {
    for (name, field) in &schema.fields {
        let path = join_context_path(prefix, name);
        let required = inherited_required && field.required;
        let nullable = inherited_nullable || field.nullable;
        let sensitivity = inherited_sensitivity.combine(field.sensitivity);
        if sensitivity.is_secret() {
            continue;
        }
        if !condition_type_contains_secret(&field.value_type, sensitivity) {
            let reference = path
                .split('.')
                .fold(FieldRef::step(step_id), |reference, segment| {
                    reference.field(segment)
                });
            output.push(ComposerConditionField {
                reference,
                label: format!(
                    "{step_id}.{path} · {}",
                    context_type_label(&field.value_type, nullable, !required)
                ),
                value_type: field.value_type.clone(),
                required,
                nullable,
            });
        }
        if let ContextType::Object { schema } = &field.value_type {
            collect_condition_fields(
                step_id,
                schema,
                &path,
                required,
                nullable,
                sensitivity,
                output,
            );
        }
    }
}

#[cfg(test)]
fn condition_type_contains_secret(
    value_type: &ContextType,
    inherited_sensitivity: Sensitivity,
) -> bool {
    match value_type {
        ContextType::Array { items } => {
            condition_type_contains_secret(items, inherited_sensitivity)
        }
        ContextType::Object { schema } => schema.fields.values().any(|field| {
            let sensitivity = inherited_sensitivity.combine(field.sensitivity);
            sensitivity.is_secret()
                || condition_type_contains_secret(&field.value_type, sensitivity)
        }),
        ContextType::Any
        | ContextType::Null
        | ContextType::Boolean
        | ContextType::Integer
        | ContextType::Number
        | ContextType::String { .. } => inherited_sensitivity.is_secret(),
    }
}

fn condition_operators(value_type: &ContextType) -> Vec<ComposerConditionOperator> {
    use ComposerConditionOperator as Operator;

    let mut operators = match value_type {
        ContextType::Any => vec![
            Operator::Equal,
            Operator::NotEqual,
            Operator::Contains,
            Operator::StartsWith,
            Operator::EndsWith,
            Operator::Matches,
            Operator::IsEmpty,
            Operator::LessThan,
            Operator::LessThanOrEqual,
            Operator::GreaterThan,
            Operator::GreaterThanOrEqual,
        ],
        ContextType::Null | ContextType::Boolean => {
            vec![Operator::Equal, Operator::NotEqual]
        }
        ContextType::Integer | ContextType::Number => vec![
            Operator::Equal,
            Operator::NotEqual,
            Operator::LessThan,
            Operator::LessThanOrEqual,
            Operator::GreaterThan,
            Operator::GreaterThanOrEqual,
        ],
        ContextType::String { .. } => vec![
            Operator::Equal,
            Operator::NotEqual,
            Operator::Contains,
            Operator::StartsWith,
            Operator::EndsWith,
            Operator::Matches,
            Operator::IsEmpty,
        ],
        ContextType::Array { .. } | ContextType::Object { .. } => vec![Operator::IsEmpty],
    };
    operators.extend([Operator::Exists, Operator::IsNull]);
    operators
}

fn condition_literal_kinds(
    field: &ComposerConditionField,
    operator: ComposerConditionOperator,
) -> Vec<ComposerLiteralKind> {
    use ComposerConditionOperator as Operator;
    use ComposerLiteralKind as Literal;

    if !operator.requires_literal() {
        return Vec::new();
    }
    let mut kinds = match operator {
        Operator::Contains | Operator::StartsWith | Operator::EndsWith | Operator::Matches => {
            vec![Literal::String]
        }
        Operator::LessThan
        | Operator::LessThanOrEqual
        | Operator::GreaterThan
        | Operator::GreaterThanOrEqual => match &field.value_type {
            ContextType::Integer => vec![Literal::Integer],
            ContextType::Number => vec![Literal::Number],
            ContextType::Any => vec![Literal::Integer, Literal::Number],
            _ => Vec::new(),
        },
        Operator::Equal | Operator::NotEqual => match &field.value_type {
            ContextType::Any => vec![
                Literal::Bool,
                Literal::String,
                Literal::Integer,
                Literal::Number,
            ],
            ContextType::Null => vec![Literal::Null],
            ContextType::Boolean => vec![Literal::Bool],
            ContextType::Integer => vec![Literal::Integer],
            ContextType::Number => vec![Literal::Number],
            ContextType::String { .. } => vec![Literal::String],
            ContextType::Array { .. } | ContextType::Object { .. } => Vec::new(),
        },
        Operator::Exists | Operator::IsNull | Operator::IsEmpty => Vec::new(),
    };
    if field.nullable
        && matches!(operator, Operator::Equal | Operator::NotEqual)
        && !kinds.contains(&Literal::Null)
    {
        kinds.push(Literal::Null);
    }
    kinds
}

fn default_condition_literal(
    field: &ComposerConditionField,
    operator: ComposerConditionOperator,
) -> Option<ExpressionValue> {
    condition_literal_kinds(field, operator)
        .into_iter()
        .next()
        .map(ComposerLiteralKind::default_value)
}

fn default_simple_condition(field: &ComposerConditionField) -> SimpleConditionRule {
    let operator = condition_operators(&field.value_type)
        .into_iter()
        .next()
        .unwrap_or(ComposerConditionOperator::Exists);
    SimpleConditionRule {
        field: field.reference.clone(),
        operator,
        literal: default_condition_literal(field, operator),
    }
}

fn default_condition_field(fields: &[ComposerConditionField]) -> Option<&ComposerConditionField> {
    fields
        .iter()
        .find(|field| {
            !matches!(
                &field.value_type,
                ContextType::Array { .. } | ContextType::Object { .. }
            )
        })
        .or_else(|| fields.first())
}

fn context_reference_expression(field: &FieldRef) -> ExpressionV1 {
    ExpressionV1::Ref {
        reference: ReferenceV1::Context {
            field: field.clone(),
        },
    }
}

fn build_simple_condition_rule(rule: &SimpleConditionRule) -> ExpressionV1 {
    let reference = || ReferenceV1::Context {
        field: rule.field.clone(),
    };
    let value = || Box::new(context_reference_expression(&rule.field));
    let literal = || {
        Box::new(ExpressionV1::Literal {
            value: rule.literal.clone().unwrap_or(ExpressionValue::Null),
        })
    };
    match rule.operator {
        ComposerConditionOperator::Exists => ExpressionV1::Exists {
            reference: reference(),
        },
        ComposerConditionOperator::IsNull => ExpressionV1::IsNull {
            expression: value(),
        },
        ComposerConditionOperator::IsEmpty => ExpressionV1::IsEmpty {
            expression: value(),
        },
        ComposerConditionOperator::Equal
        | ComposerConditionOperator::NotEqual
        | ComposerConditionOperator::LessThan
        | ComposerConditionOperator::LessThanOrEqual
        | ComposerConditionOperator::GreaterThan
        | ComposerConditionOperator::GreaterThanOrEqual => ExpressionV1::Compare {
            operator: match rule.operator {
                ComposerConditionOperator::Equal => ComparisonOperator::Equal,
                ComposerConditionOperator::NotEqual => ComparisonOperator::NotEqual,
                ComposerConditionOperator::LessThan => ComparisonOperator::LessThan,
                ComposerConditionOperator::LessThanOrEqual => ComparisonOperator::LessThanOrEqual,
                ComposerConditionOperator::GreaterThan => ComparisonOperator::GreaterThan,
                ComposerConditionOperator::GreaterThanOrEqual => {
                    ComparisonOperator::GreaterThanOrEqual
                }
                _ => unreachable!(),
            },
            left: value(),
            right: literal(),
        },
        ComposerConditionOperator::Contains => ExpressionV1::Contains {
            value: value(),
            needle: literal(),
        },
        ComposerConditionOperator::StartsWith => ExpressionV1::StartsWith {
            value: value(),
            prefix: literal(),
        },
        ComposerConditionOperator::EndsWith => ExpressionV1::EndsWith {
            value: value(),
            suffix: literal(),
        },
        ComposerConditionOperator::Matches => ExpressionV1::Matches {
            value: value(),
            pattern: match rule.literal.as_ref() {
                Some(ExpressionValue::String(pattern)) => pattern.clone(),
                _ => String::new(),
            },
        },
    }
}

fn simple_condition_rule(expression: &ExpressionV1) -> Option<SimpleConditionRule> {
    fn context_field(expression: &ExpressionV1) -> Option<&FieldRef> {
        match expression {
            ExpressionV1::Ref {
                reference: ReferenceV1::Context { field },
            } => Some(field),
            _ => None,
        }
    }

    fn literal(expression: &ExpressionV1) -> Option<&ExpressionValue> {
        match expression {
            ExpressionV1::Literal { value } => Some(value),
            _ => None,
        }
    }

    match expression {
        ExpressionV1::Exists {
            reference: ReferenceV1::Context { field },
        } => Some(SimpleConditionRule {
            field: field.clone(),
            operator: ComposerConditionOperator::Exists,
            literal: None,
        }),
        ExpressionV1::IsNull { expression } => Some(SimpleConditionRule {
            field: context_field(expression)?.clone(),
            operator: ComposerConditionOperator::IsNull,
            literal: None,
        }),
        ExpressionV1::IsEmpty { expression } => Some(SimpleConditionRule {
            field: context_field(expression)?.clone(),
            operator: ComposerConditionOperator::IsEmpty,
            literal: None,
        }),
        ExpressionV1::Compare {
            operator,
            left,
            right,
        } => Some(SimpleConditionRule {
            field: context_field(left)?.clone(),
            operator: match operator {
                ComparisonOperator::Equal => ComposerConditionOperator::Equal,
                ComparisonOperator::NotEqual => ComposerConditionOperator::NotEqual,
                ComparisonOperator::LessThan => ComposerConditionOperator::LessThan,
                ComparisonOperator::LessThanOrEqual => ComposerConditionOperator::LessThanOrEqual,
                ComparisonOperator::GreaterThan => ComposerConditionOperator::GreaterThan,
                ComparisonOperator::GreaterThanOrEqual => {
                    ComposerConditionOperator::GreaterThanOrEqual
                }
            },
            literal: Some(literal(right)?.clone()),
        }),
        ExpressionV1::Contains { value, needle } => Some(SimpleConditionRule {
            field: context_field(value)?.clone(),
            operator: ComposerConditionOperator::Contains,
            literal: Some(literal(needle)?.clone()),
        }),
        ExpressionV1::StartsWith { value, prefix } => Some(SimpleConditionRule {
            field: context_field(value)?.clone(),
            operator: ComposerConditionOperator::StartsWith,
            literal: Some(literal(prefix)?.clone()),
        }),
        ExpressionV1::EndsWith { value, suffix } => Some(SimpleConditionRule {
            field: context_field(value)?.clone(),
            operator: ComposerConditionOperator::EndsWith,
            literal: Some(literal(suffix)?.clone()),
        }),
        ExpressionV1::Matches { value, pattern } => Some(SimpleConditionRule {
            field: context_field(value)?.clone(),
            operator: ComposerConditionOperator::Matches,
            literal: Some(ExpressionValue::String(pattern.clone())),
        }),
        _ => None,
    }
}

fn simple_condition_rule_supported(
    rule: &SimpleConditionRule,
    fields: &[ComposerConditionField],
) -> bool {
    let Some(field) = fields.iter().find(|field| field.reference == rule.field) else {
        // Keep a now-invisible stable reference editable so the user can
        // explicitly replace it with one of the preceding fields.
        return true;
    };
    if !condition_operators(&field.value_type).contains(&rule.operator) {
        return false;
    }
    if !rule.operator.requires_literal() {
        return rule.literal.is_none();
    }
    let Some(kind) = rule
        .literal
        .as_ref()
        .and_then(ComposerLiteralKind::from_value)
    else {
        return false;
    };
    condition_literal_kinds(field, rule.operator).contains(&kind)
}

fn composer_condition_rule(expression: &ExpressionV1) -> Option<ComposerConditionRule> {
    fn parse(
        expression: &ExpressionV1,
        depth: usize,
        nodes: &mut usize,
    ) -> Option<ComposerConditionRule> {
        if depth > CONDITION_EDITOR_MAX_DEPTH || *nodes >= CONDITION_EDITOR_MAX_NODES {
            return None;
        }
        *nodes += 1;
        if let Some(rule) = simple_condition_rule(expression) {
            return Some(ComposerConditionRule::Clause(rule));
        }
        match expression {
            ExpressionV1::All { expressions } if !expressions.is_empty() => {
                let mut rules = Vec::with_capacity(
                    expressions
                        .len()
                        .min(CONDITION_EDITOR_MAX_NODES.saturating_sub(*nodes)),
                );
                for expression in expressions {
                    rules.push(parse(expression, depth + 1, nodes)?);
                }
                Some(ComposerConditionRule::All(rules))
            }
            ExpressionV1::Any { expressions } if !expressions.is_empty() => {
                let mut rules = Vec::with_capacity(
                    expressions
                        .len()
                        .min(CONDITION_EDITOR_MAX_NODES.saturating_sub(*nodes)),
                );
                for expression in expressions {
                    rules.push(parse(expression, depth + 1, nodes)?);
                }
                Some(ComposerConditionRule::Any(rules))
            }
            ExpressionV1::Not { expression } => Some(ComposerConditionRule::Not(Box::new(parse(
                expression,
                depth + 1,
                nodes,
            )?))),
            // Quantifiers, `in`, value expressions, and future AST variants
            // are deliberately not approximated by this editor.
            _ => None,
        }
    }

    let mut nodes = 0;
    parse(expression, 0, &mut nodes)
}

fn build_composer_condition_rule(rule: &ComposerConditionRule) -> ExpressionV1 {
    match rule {
        ComposerConditionRule::Clause(rule) => build_simple_condition_rule(rule),
        ComposerConditionRule::All(rules) => ExpressionV1::All {
            expressions: rules.iter().map(build_composer_condition_rule).collect(),
        },
        ComposerConditionRule::Any(rules) => ExpressionV1::Any {
            expressions: rules.iter().map(build_composer_condition_rule).collect(),
        },
        ComposerConditionRule::Not(rule) => ExpressionV1::Not {
            expression: Box::new(build_composer_condition_rule(rule)),
        },
    }
}

fn condition_read_only_summary(expression: &ExpressionV1) -> String {
    const MAX_CHARS: usize = 480;
    let source = serde_yaml::to_string(expression)
        .unwrap_or_else(|error| format!("<condition serialization failed: {error}>"));
    let mut chars = source.trim().chars();
    let summary = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{summary}\n…")
    } else {
        summary
    }
}

fn default_graph_if_condition(fields: &[ComposerConditionField]) -> ExpressionV1 {
    default_condition_field(fields).map_or(
        ExpressionV1::Literal {
            value: ExpressionValue::Bool(true),
        },
        |field| {
            build_composer_condition_rule(&ComposerConditionRule::Clause(default_simple_condition(
                field,
            )))
        },
    )
}

fn composer_condition_rule_supported(
    rule: &ComposerConditionRule,
    fields: &[ComposerConditionField],
) -> bool {
    match rule {
        ComposerConditionRule::Clause(rule) => simple_condition_rule_supported(rule, fields),
        ComposerConditionRule::All(rules) | ComposerConditionRule::Any(rules) => {
            !rules.is_empty()
                && rules
                    .iter()
                    .all(|rule| composer_condition_rule_supported(rule, fields))
        }
        ComposerConditionRule::Not(rule) => composer_condition_rule_supported(rule, fields),
    }
}

fn composer_condition_rule_nodes(rule: &ComposerConditionRule) -> usize {
    match rule {
        ComposerConditionRule::Clause(_) => 1,
        ComposerConditionRule::All(rules) | ComposerConditionRule::Any(rules) => {
            1 + rules
                .iter()
                .map(composer_condition_rule_nodes)
                .sum::<usize>()
        }
        ComposerConditionRule::Not(rule) => 1 + composer_condition_rule_nodes(rule),
    }
}

fn composer_condition_rule_depth(rule: &ComposerConditionRule) -> usize {
    match rule {
        ComposerConditionRule::Clause(_) => 0,
        ComposerConditionRule::All(rules) | ComposerConditionRule::Any(rules) => rules
            .iter()
            .map(composer_condition_rule_depth)
            .max()
            .map_or(0, |depth| depth.saturating_add(1)),
        ComposerConditionRule::Not(rule) => composer_condition_rule_depth(rule).saturating_add(1),
    }
}

fn composer_condition_rule_fits_editor(rule: &ComposerConditionRule) -> bool {
    composer_condition_rule_nodes(rule) <= CONDITION_EDITOR_MAX_NODES
        && composer_condition_rule_depth(rule) <= CONDITION_EDITOR_MAX_DEPTH
}

fn composer_condition_replacement_fits(
    current: &ComposerConditionRule,
    replacement: &ComposerConditionRule,
    depth: usize,
    total_nodes: usize,
) -> bool {
    let projected_nodes = total_nodes
        .saturating_sub(composer_condition_rule_nodes(current))
        .saturating_add(composer_condition_rule_nodes(replacement));
    projected_nodes <= CONDITION_EDITOR_MAX_NODES
        && depth.saturating_add(composer_condition_rule_depth(replacement))
            <= CONDITION_EDITOR_MAX_DEPTH
}

fn regex_pattern_error(pattern: &str) -> Option<String> {
    let limits = ExpressionLimits::default();
    if pattern.len() > limits.max_regex_pattern_bytes {
        return Some(format!(
            "Шаблон занимает {} байт; максимум — {}.",
            pattern.len(),
            limits.max_regex_pattern_bytes
        ));
    }
    RegexBuilder::new(pattern)
        .size_limit(limits.max_regex_compiled_bytes)
        .dfa_size_limit(limits.max_regex_compiled_bytes)
        .build()
        .err()
        .map(|error| format!("Некорректное регулярное выражение: {error}"))
}

fn collect_schema_arrays(
    schema: &ObjectSchema,
    prefix: &str,
    output: &mut Vec<(String, ContextType)>,
) {
    for (name, field) in &schema.fields {
        let path = join_context_path(prefix, name);
        match &field.value_type {
            ContextType::Array { items } => output.push((path, items.as_ref().clone())),
            ContextType::Object { schema } => collect_schema_arrays(schema, &path, output),
            _ => {}
        }
    }
}

fn join_context_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.into()
    } else {
        format!("{prefix}.{field}")
    }
}

fn item_alias_for_array_path(path: &str) -> String {
    let candidate = path.rsplit('.').next().unwrap_or("item");
    let singular = candidate
        .strip_suffix("ies")
        .map(|stem| format!("{stem}y"))
        .or_else(|| candidate.strip_suffix('s').map(str::to_owned))
        .unwrap_or_else(|| candidate.to_owned());
    let sanitized = singular
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "item".into()
    } else {
        sanitized
    }
}

#[cfg(test)]
fn project_item_type(item_type: &ContextType, fields: &[String]) -> ContextType {
    if fields.is_empty() {
        return item_type.clone();
    }
    let ContextType::Object { schema } = item_type else {
        return item_type.clone();
    };
    let mut projected = schema.as_ref().clone();
    projected
        .fields
        .retain(|name, _| fields.iter().any(|field| field == name));
    ContextType::object(projected)
}

#[cfg(test)]
fn item_object_fields(item_type: &ContextType) -> Vec<(String, FieldSchema)> {
    match item_type {
        ContextType::Object { schema } => schema
            .fields
            .iter()
            .map(|(name, field)| (name.clone(), field.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn clone_item_field_names(fields: &[(String, FieldSchema)]) -> Vec<String> {
    fields
        .iter()
        .filter(|(_, field)| match &field.value_type {
            ContextType::String {
                format: Some(format),
            } => matches!(
                format,
                SemanticFormat::GitUrl | SemanticFormat::GitRef | SemanticFormat::RepositoryName
            ),
            _ => false,
        })
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
fn composer_context_options(
    source: &ComposerLoopSource,
    expected: &ContextType,
) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    collect_bindable_fields(&source.item_type, "", &mut fields);
    fields
        .into_iter()
        .filter(|(_, value_type)| expected.is_assignable_from(value_type))
        .map(|(path, value_type)| {
            let expression = if path.is_empty() {
                source.item.clone()
            } else {
                format!("{}.{}", source.item, path)
            };
            (
                format!(
                    "{} · {}",
                    expression,
                    context_type_label(&value_type, false, false)
                ),
                format!("{{{{{expression}}}}}"),
            )
        })
        .collect()
}

#[cfg(test)]
fn composer_destination_options(
    source: &ComposerLoopSource,
    expected: &ContextType,
) -> Vec<(String, String)> {
    let mut options = composer_context_options(source, expected);
    let mut fields = Vec::new();
    collect_bindable_fields(&source.item_type, "", &mut fields);
    options.extend(
        fields
            .into_iter()
            .filter(|(_, value_type)| {
                matches!(
                    value_type,
                    ContextType::String {
                        format: Some(SemanticFormat::RepositoryName)
                    }
                )
            })
            .map(|(path, _)| {
                let expression = format!("{}.{}", source.item, path);
                (
                    format!("$HOME/Developer/{expression}"),
                    format!("$HOME/Developer/{{{{{expression}}}}}"),
                )
            }),
    );
    let mut seen = BTreeSet::new();
    options.retain(|(_, template)| seen.insert(template.clone()));
    options
}

#[cfg(test)]
fn collect_bindable_fields(
    value_type: &ContextType,
    prefix: &str,
    output: &mut Vec<(String, ContextType)>,
) {
    match value_type {
        ContextType::Object { schema } => {
            for (name, field) in &schema.fields {
                let path = join_context_path(prefix, name);
                match &field.value_type {
                    ContextType::Object { .. } => {
                        collect_bindable_fields(&field.value_type, &path, output)
                    }
                    ContextType::Array { .. } => {}
                    _ => output.push((path, field.value_type.clone())),
                }
            }
        }
        ContextType::Array { .. } => {}
        _ if prefix.is_empty() => output.push((String::new(), value_type.clone())),
        _ => output.push((prefix.into(), value_type.clone())),
    }
}

#[cfg(test)]
fn composer_indexed_field_options(
    source: &ComposerArraySource,
    expected: &FieldSchema,
) -> Vec<ComposerBindableField> {
    composer_typed_field_options(&source.item_type, expected)
}

#[cfg(test)]
fn composer_loop_field_options(
    source: &ComposerLoopSource,
    expected: &FieldSchema,
) -> Vec<ComposerBindableField> {
    composer_typed_field_options(&source.item_type, expected)
}

#[cfg(test)]
fn composer_typed_field_options(
    item_type: &ContextType,
    expected: &FieldSchema,
) -> Vec<ComposerBindableField> {
    let mut fields = Vec::new();
    collect_bindable_field_details(item_type, "", true, false, Sensitivity::Public, &mut fields);
    fields.retain(|field| {
        expected.value_type.is_assignable_from(&field.value_type)
            && (!expected.required || field.required)
            && (expected.nullable || !field.nullable)
            && (!field.sensitivity.is_secret() || expected.sensitivity.is_secret())
    });
    fields
}

fn collect_bindable_field_details(
    value_type: &ContextType,
    prefix: &str,
    inherited_required: bool,
    inherited_nullable: bool,
    inherited_sensitivity: Sensitivity,
    output: &mut Vec<ComposerBindableField>,
) {
    match value_type {
        ContextType::Object { schema } => {
            for (name, field) in &schema.fields {
                let path = join_context_path(prefix, name);
                let required = inherited_required && field.required;
                let nullable = inherited_nullable || field.nullable;
                let sensitivity = inherited_sensitivity.combine(field.sensitivity);
                match &field.value_type {
                    ContextType::Object { .. } => collect_bindable_field_details(
                        &field.value_type,
                        &path,
                        required,
                        nullable,
                        sensitivity,
                        output,
                    ),
                    ContextType::Array { .. } => {}
                    _ => output.push(ComposerBindableField {
                        path,
                        value_type: field.value_type.clone(),
                        required,
                        nullable,
                        sensitivity,
                    }),
                }
            }
        }
        ContextType::Array { .. } => {}
        _ => output.push(ComposerBindableField {
            path: prefix.into(),
            value_type: value_type.clone(),
            required: inherited_required,
            nullable: inherited_nullable,
            sensitivity: inherited_sensitivity,
        }),
    }
}

#[cfg(test)]
fn composer_indexed_binding(
    source: &ComposerArraySource,
    index: usize,
    field_path: &str,
) -> Binding {
    let mut reference = FieldRef::step(&source.step_id);
    for segment in source.path.split('.').filter(|segment| !segment.is_empty()) {
        reference = reference.field(segment);
    }
    reference = reference.index(index);
    for segment in field_path.split('.').filter(|segment| !segment.is_empty()) {
        reference = reference.field(segment);
    }
    Binding::field(reference)
}

#[cfg(test)]
fn composer_indexed_binding_selection(
    binding: &Binding,
    sources: &[ComposerArraySource],
) -> Option<ComposerIndexedBinding> {
    let Binding::Field { field } = binding else {
        return None;
    };
    let ContextScope::Step { step_id } = &field.scope else {
        return None;
    };
    for source in sources.iter().filter(|source| source.step_id == *step_id) {
        let array_segments = source
            .path
            .split('.')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if field.segments.len() <= array_segments.len() {
            continue;
        }
        let prefix_matches = array_segments.iter().enumerate().all(|(index, expected)| {
            matches!(
                field.segments.get(index),
                Some(ContextPathSegment::Field { name }) if name == expected
            )
        });
        if !prefix_matches {
            continue;
        }
        let Some(ContextPathSegment::Index { index }) = field.segments.get(array_segments.len())
        else {
            continue;
        };
        let mut field_names = Vec::new();
        let mut supported = true;
        for segment in field.segments.iter().skip(array_segments.len() + 1) {
            match segment {
                ContextPathSegment::Field { name } => field_names.push(name.clone()),
                ContextPathSegment::Index { .. } => {
                    supported = false;
                    break;
                }
            }
        }
        if supported {
            return Some(ComposerIndexedBinding {
                source_step: source.step_id.clone(),
                array_path: source.path.clone(),
                index: *index,
                field_path: field_names.join("."),
            });
        }
    }
    None
}

#[cfg(test)]
fn composer_indexed_binding_preview(selection: &ComposerIndexedBinding, target: &str) -> String {
    let field = if selection.field_path.is_empty() {
        String::new()
    } else {
        format!(".{}", selection.field_path)
    };
    format!(
        "{}.{}[{}]{} → {}",
        selection.source_step,
        selection.array_path,
        selection.index.saturating_add(1),
        field,
        target
    )
}

#[cfg(test)]
fn composer_loop_field_ref(source: &ComposerLoopSource, field_path: &str) -> FieldRef {
    let mut reference = FieldRef::loop_item(&source.step_id);
    for segment in field_path.split('.').filter(|segment| !segment.is_empty()) {
        reference = reference.field(segment);
    }
    reference
}

#[cfg(test)]
fn composer_loop_binding(source: &ComposerLoopSource, field_path: &str) -> Binding {
    Binding::field(composer_loop_field_ref(source, field_path))
}

#[cfg(test)]
fn composer_insert_default_loop_binding(
    bindings: &mut BTreeMap<String, Binding>,
    target: &str,
    source: &ComposerLoopSource,
    expected: &FieldSchema,
) -> bool {
    if bindings.contains_key(target) {
        return false;
    }
    let Some(field) = composer_loop_field_options(source, expected)
        .into_iter()
        .next()
    else {
        return false;
    };
    bindings.insert(target.into(), composer_loop_binding(source, &field.path));
    true
}

#[cfg(test)]
fn composer_loop_destination_suffixes(
    source: &ComposerLoopSource,
) -> Vec<ComposerLoopDestinationSuffix> {
    let mut fields = Vec::new();
    collect_bindable_field_details(
        &source.item_type,
        "",
        true,
        false,
        Sensitivity::Public,
        &mut fields,
    );
    let repository_names = fields
        .into_iter()
        .filter(|field| {
            field.required
                && !field.nullable
                && !field.sensitivity.is_secret()
                && matches!(
                    field.value_type,
                    ContextType::String {
                        format: Some(SemanticFormat::RepositoryName)
                    }
                )
        })
        .map(|field| field.path)
        .collect::<BTreeSet<_>>();

    let mut suffixes = Vec::new();
    if repository_names.contains("full_name") {
        suffixes.push(ComposerLoopDestinationSuffix::FullName {
            field_path: "full_name".into(),
        });
    }
    if repository_names.contains("owner") && repository_names.contains("name") {
        suffixes.push(ComposerLoopDestinationSuffix::OwnerName {
            owner_path: "owner".into(),
            name_path: "name".into(),
        });
    }
    suffixes
}

#[cfg(test)]
fn composer_destination_root_literal(root: &str) -> String {
    let root = root.trim();
    if root.is_empty() {
        String::new()
    } else if root == "/" || root.ends_with('/') {
        root.into()
    } else {
        format!("{root}/")
    }
}

#[cfg(test)]
fn composer_destination_root_error(root: &str) -> Option<&'static str> {
    let root = root.trim();
    if root.is_empty() {
        return Some("Укажите абсолютный базовый каталог.");
    }
    if root.contains('\0') {
        return Some("Базовый каталог не должен содержать NUL.");
    }
    let path = if root == "$HOME" {
        Path::new("")
    } else if let Some(path) = root.strip_prefix("$HOME/") {
        Path::new(path)
    } else if Path::new(root).is_absolute() {
        Path::new(root)
    } else {
        return Some("Используйте абсолютный путь либо $HOME/…");
    };
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Some("Базовый каталог не должен содержать '..'.");
    }
    None
}

#[cfg(test)]
fn composer_loop_destination_binding(
    source: &ComposerLoopSource,
    root: &str,
    suffix: &ComposerLoopDestinationSuffix,
) -> Binding {
    let mut parts = vec![TemplatePart::literal(composer_destination_root_literal(
        root,
    ))];
    match suffix {
        ComposerLoopDestinationSuffix::FullName { field_path } => {
            parts.push(TemplatePart::field(composer_loop_field_ref(
                source, field_path,
            )));
        }
        ComposerLoopDestinationSuffix::OwnerName {
            owner_path,
            name_path,
        } => {
            parts.push(TemplatePart::field(composer_loop_field_ref(
                source, owner_path,
            )));
            parts.push(TemplatePart::literal("/"));
            parts.push(TemplatePart::field(composer_loop_field_ref(
                source, name_path,
            )));
        }
    }
    Binding::interpolated(parts)
}

#[cfg(test)]
fn composer_loop_destination_binding_selection(
    binding: &Binding,
    source: &ComposerLoopSource,
) -> Option<ComposerLoopDestinationBinding> {
    let Binding::Interpolated { parts } = binding else {
        return None;
    };
    let Some(TemplatePart::Literal { value }) = parts.first() else {
        return None;
    };
    let root: String = if value.is_empty() {
        String::new()
    } else if value == "/" {
        "/".into()
    } else {
        value.strip_suffix('/')?.to_owned()
    };
    composer_loop_destination_suffixes(source)
        .into_iter()
        .find_map(|suffix| {
            (composer_loop_destination_binding(source, &root, &suffix) == *binding).then_some(
                ComposerLoopDestinationBinding {
                    root: root.clone(),
                    suffix,
                },
            )
        })
}

#[cfg(test)]
#[allow(dead_code)]
fn composer_loop_destination_suffix_label(
    source: &ComposerLoopSource,
    suffix: &ComposerLoopDestinationSuffix,
) -> String {
    match suffix {
        ComposerLoopDestinationSuffix::FullName { field_path } => {
            format!("{}.{}", source.item, field_path)
        }
        ComposerLoopDestinationSuffix::OwnerName {
            owner_path,
            name_path,
        } => format!(
            "{}.{} / {}.{}",
            source.item, owner_path, source.item, name_path
        ),
    }
}

#[cfg(test)]
fn composer_loop_destination_preview(
    source: &ComposerLoopSource,
    selection: &ComposerLoopDestinationBinding,
) -> String {
    let root = composer_destination_root_literal(&selection.root);
    match &selection.suffix {
        ComposerLoopDestinationSuffix::FullName { field_path } => {
            format!("{root}{{{{{}.{field_path}}}}} → dest", source.item)
        }
        ComposerLoopDestinationSuffix::OwnerName {
            owner_path,
            name_path,
        } => format!(
            "{root}{{{{{}.{owner_path}}}}}/{{{{{}.{name_path}}}}} → dest",
            source.item, source.item
        ),
    }
}

#[cfg(test)]
fn composer_loop_binding_selection(
    binding: &Binding,
    sources: &[ComposerLoopSource],
) -> Option<ComposerLoopBinding> {
    let Binding::Field { field } = binding else {
        return None;
    };
    let ContextScope::LoopItem { step_id } = &field.scope else {
        return None;
    };
    if !sources.iter().any(|source| source.step_id == *step_id) {
        return None;
    }
    let mut field_names = Vec::new();
    for segment in &field.segments {
        match segment {
            ContextPathSegment::Field { name } => field_names.push(name.clone()),
            ContextPathSegment::Index { .. } => return None,
        }
    }
    Some(ComposerLoopBinding {
        loop_step: step_id.clone(),
        field_path: field_names.join("."),
    })
}

#[cfg(test)]
fn composer_indexed_binding_for_loop(
    binding: &Binding,
    loop_source: &ComposerLoopSource,
    array_sources: &[ComposerArraySource],
) -> Option<ComposerLoopBinding> {
    let indexed = composer_indexed_binding_selection(binding, array_sources)?;
    (indexed.source_step == loop_source.source_step && indexed.array_path == loop_source.array_path)
        .then_some(ComposerLoopBinding {
            loop_step: loop_source.step_id.clone(),
            field_path: indexed.field_path,
        })
}

#[cfg(test)]
fn composer_loop_binding_preview(
    source: &ComposerLoopSource,
    selection: &ComposerLoopBinding,
    target: &str,
) -> String {
    let field = if selection.field_path.is_empty() {
        String::new()
    } else {
        format!(".{}", selection.field_path)
    };
    format!("{}{field} → {target}", source.item)
}

#[cfg(test)]
impl ComposerBlockKind {
    const ALL: [Self; 12] = [
        Self::GithubListRepositories,
        Self::ForEach,
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

    fn action_kind(self) -> ActionKind {
        match self {
            Self::GithubListRepositories => ActionKind::GithubListRepositories,
            Self::ForEach => ActionKind::ForEach,
            Self::GitInspect => ActionKind::GitInspect,
            Self::GitCloneIfMissing => ActionKind::GitCloneIfMissing,
            Self::GitFetch => ActionKind::GitFetch,
            Self::GitFastForward => ActionKind::GitFastForward,
            Self::CreateDirectory => ActionKind::CreateDirectory,
            Self::InspectPath => ActionKind::InspectPath,
            Self::CopyPath => ActionKind::CopyPath,
            Self::WriteFile => ActionKind::WriteFile,
            Self::RemovePath => ActionKind::RemovePath,
            Self::BrewInstall => ActionKind::BrewInstall,
        }
    }
}

#[derive(Debug, Clone)]
enum ComposerGraphCard {
    Action(Box<Step>),
    ForEach {
        item_alias: String,
        body_empty: bool,
    },
    If,
    Switch,
    Join,
}

#[derive(Debug, Clone)]
struct ComposerGraphVisualNode {
    id: String,
    scope: Option<ComposerGraphNestedScope>,
    card: ComposerGraphCard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerGraphEdgeKind {
    Flow,
    Iteration,
    Then,
    Else,
    Case,
    Default,
}

#[derive(Debug, Clone)]
struct ComposerGraphVisualEdge {
    from: String,
    to: String,
    kind: ComposerGraphEdgeKind,
    port: Option<EdgePort>,
}

#[derive(Debug, Clone)]
struct ComposerGraphBindingOption {
    label: String,
    binding: Binding,
    value_type: ContextType,
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
}

fn graph_node_ids(graph: &WorkflowGraph) -> BTreeSet<String> {
    fn visit(graph: &WorkflowGraph, ids: &mut BTreeSet<String>) {
        for node in &graph.nodes {
            ids.insert(node.id().to_owned());
            match node {
                GraphNode::ForEach(node) => visit(&node.body, ids),
                GraphNode::If(node) => {
                    visit(&node.then_graph, ids);
                    if let Some(graph) = &node.else_graph {
                        visit(graph, ids);
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        visit(&case.graph, ids);
                    }
                    if let Some(graph) = &node.default {
                        visit(graph, ids);
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
    }

    let mut ids = BTreeSet::new();
    visit(graph, &mut ids);
    ids
}

fn graph_node<'a>(graph: &'a WorkflowGraph, id: &str) -> Option<&'a GraphNode> {
    for node in &graph.nodes {
        if node.id() == id {
            return Some(node);
        }
        let found = match node {
            GraphNode::ForEach(node) => graph_node(&node.body, id),
            GraphNode::If(node) => graph_node(&node.then_graph, id).or_else(|| {
                node.else_graph
                    .as_deref()
                    .and_then(|graph| graph_node(graph, id))
            }),
            GraphNode::Switch(node) => node
                .cases
                .iter()
                .find_map(|case| graph_node(&case.graph, id))
                .or_else(|| {
                    node.default
                        .as_deref()
                        .and_then(|graph| graph_node(graph, id))
                }),
            GraphNode::Action(_) | GraphNode::Join(_) => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn graph_node_mut<'a>(graph: &'a mut WorkflowGraph, id: &str) -> Option<&'a mut GraphNode> {
    for node in &mut graph.nodes {
        if node.id() == id {
            return Some(node);
        }
        let found = match node {
            GraphNode::ForEach(node) => graph_node_mut(&mut node.body, id),
            GraphNode::If(node) => graph_node_mut(&mut node.then_graph, id).or_else(|| {
                node.else_graph
                    .as_deref_mut()
                    .and_then(|graph| graph_node_mut(graph, id))
            }),
            GraphNode::Switch(node) => node
                .cases
                .iter_mut()
                .find_map(|case| graph_node_mut(&mut case.graph, id))
                .or_else(|| {
                    node.default
                        .as_deref_mut()
                        .and_then(|graph| graph_node_mut(graph, id))
                }),
            GraphNode::Action(_) | GraphNode::Join(_) => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn graph_for_each_body_mut<'a>(
    graph: &'a mut WorkflowGraph,
    loop_id: &str,
) -> Option<&'a mut WorkflowGraph> {
    let GraphNode::ForEach(node) = graph_node_mut(graph, loop_id)? else {
        return None;
    };
    Some(&mut node.body)
}

fn graph_nested_scope_mut<'a>(
    graph: &'a mut WorkflowGraph,
    scope: &ComposerGraphNestedScope,
) -> Option<&'a mut WorkflowGraph> {
    match scope {
        ComposerGraphNestedScope::ForEachBody { owner_id } => {
            graph_for_each_body_mut(graph, owner_id)
        }
        ComposerGraphNestedScope::IfThen { owner_id } => {
            let GraphNode::If(node) = graph_node_mut(graph, owner_id)? else {
                return None;
            };
            Some(&mut node.then_graph)
        }
        ComposerGraphNestedScope::IfElse { owner_id } => {
            let GraphNode::If(node) = graph_node_mut(graph, owner_id)? else {
                return None;
            };
            Some(node.else_graph.get_or_insert_with(Default::default))
        }
        ComposerGraphNestedScope::SwitchCase { owner_id, case_id } => {
            let GraphNode::Switch(node) = graph_node_mut(graph, owner_id)? else {
                return None;
            };
            node.cases
                .iter_mut()
                .find(|case| case.id == *case_id)
                .map(|case| case.graph.as_mut())
        }
        ComposerGraphNestedScope::SwitchDefault { owner_id } => {
            let GraphNode::Switch(node) = graph_node_mut(graph, owner_id)? else {
                return None;
            };
            Some(node.default.get_or_insert_with(Default::default))
        }
    }
}

fn graph_nested_scope<'a>(
    graph: &'a WorkflowGraph,
    scope: &ComposerGraphNestedScope,
) -> Option<&'a WorkflowGraph> {
    match scope {
        ComposerGraphNestedScope::ForEachBody { owner_id } => {
            let GraphNode::ForEach(node) = graph_node(graph, owner_id)? else {
                return None;
            };
            Some(&node.body)
        }
        ComposerGraphNestedScope::IfThen { owner_id } => {
            let GraphNode::If(node) = graph_node(graph, owner_id)? else {
                return None;
            };
            Some(&node.then_graph)
        }
        ComposerGraphNestedScope::IfElse { owner_id } => {
            let GraphNode::If(node) = graph_node(graph, owner_id)? else {
                return None;
            };
            node.else_graph.as_deref()
        }
        ComposerGraphNestedScope::SwitchCase { owner_id, case_id } => {
            let GraphNode::Switch(node) = graph_node(graph, owner_id)? else {
                return None;
            };
            node.cases
                .iter()
                .find(|case| case.id == *case_id)
                .map(|case| case.graph.as_ref())
        }
        ComposerGraphNestedScope::SwitchDefault { owner_id } => {
            let GraphNode::Switch(node) = graph_node(graph, owner_id)? else {
                return None;
            };
            node.default.as_deref()
        }
    }
}

fn graph_local_scope<'a>(graph: &'a WorkflowGraph, node_id: &str) -> Option<&'a WorkflowGraph> {
    if graph.nodes.iter().any(|node| node.id() == node_id) {
        return Some(graph);
    }
    for node in &graph.nodes {
        let found = match node {
            GraphNode::ForEach(node) => graph_local_scope(&node.body, node_id),
            GraphNode::If(node) => graph_local_scope(&node.then_graph, node_id).or_else(|| {
                node.else_graph
                    .as_deref()
                    .and_then(|graph| graph_local_scope(graph, node_id))
            }),
            GraphNode::Switch(node) => node
                .cases
                .iter()
                .find_map(|case| graph_local_scope(&case.graph, node_id))
                .or_else(|| {
                    node.default
                        .as_deref()
                        .and_then(|graph| graph_local_scope(graph, node_id))
                }),
            GraphNode::Action(_) | GraphNode::Join(_) => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn graph_local_scope_mut<'a>(
    graph: &'a mut WorkflowGraph,
    node_id: &str,
) -> Option<&'a mut WorkflowGraph> {
    if graph.nodes.iter().any(|node| node.id() == node_id) {
        return Some(graph);
    }
    for node in &mut graph.nodes {
        let found = match node {
            GraphNode::ForEach(node) => graph_local_scope_mut(&mut node.body, node_id),
            GraphNode::If(node) => {
                graph_local_scope_mut(&mut node.then_graph, node_id).or_else(|| {
                    node.else_graph
                        .as_deref_mut()
                        .and_then(|graph| graph_local_scope_mut(graph, node_id))
                })
            }
            GraphNode::Switch(node) => node
                .cases
                .iter_mut()
                .find_map(|case| graph_local_scope_mut(&mut case.graph, node_id))
                .or_else(|| {
                    node.default
                        .as_deref_mut()
                        .and_then(|graph| graph_local_scope_mut(graph, node_id))
                }),
            GraphNode::Action(_) | GraphNode::Join(_) => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn graph_set_incoming_edge(
    graph: &mut WorkflowGraph,
    target_id: &str,
    source_id: &str,
    port: Option<EdgePort>,
) -> Result<(), String> {
    let scope =
        graph_local_scope(graph, target_id).ok_or_else(|| format!("узел {target_id} не найден"))?;
    let source = scope
        .nodes
        .iter()
        .find(|node| node.id() == source_id)
        .ok_or_else(|| format!("узел-источник {source_id} не найден в этой ветви"))?;
    if let Some(port) = &port {
        if !graph_node_output_ports(source).contains(port) {
            return Err(format!("порт {port:?} недоступен для узла {source_id}"));
        }
        let mut reachable = BTreeSet::from([target_id.to_owned()]);
        let mut pending = vec![target_id.to_owned()];
        while let Some(node) = pending.pop() {
            for next in scope
                .edges
                .iter()
                .filter(|edge| edge.from.node == node)
                .map(|edge| edge.to.node.clone())
            {
                if reachable.insert(next.clone()) {
                    pending.push(next);
                }
            }
        }
        if reachable.contains(source_id) {
            return Err(format!(
                "Нельзя подключить {source_id} к {target_id}: связь создаст цикл."
            ));
        }
    }
    let scope = graph_local_scope_mut(graph, target_id)
        .ok_or_else(|| format!("узел {target_id} не найден"))?;
    scope
        .edges
        .retain(|edge| !(edge.from.node == source_id && edge.to.node == target_id));
    if let Some(port) = port {
        scope.edges.push(GraphEdge::new(source_id, port, target_id));
        scope.entries.retain(|entry| entry != target_id);
    }
    Ok(())
}

fn graph_visual_model(
    graph: &WorkflowGraph,
) -> (Vec<ComposerGraphVisualNode>, Vec<ComposerGraphVisualEdge>) {
    fn visit(
        graph: &WorkflowGraph,
        scope: Option<ComposerGraphNestedScope>,
        nodes: &mut Vec<ComposerGraphVisualNode>,
        edges: &mut Vec<ComposerGraphVisualEdge>,
    ) {
        for node in &graph.nodes {
            let card = match node {
                GraphNode::Action(node) => ComposerGraphCard::Action(Box::new(node.step.clone())),
                GraphNode::ForEach(node) => ComposerGraphCard::ForEach {
                    item_alias: node.item_alias.clone(),
                    body_empty: node.body.nodes.is_empty(),
                },
                GraphNode::If(_) => ComposerGraphCard::If,
                GraphNode::Switch(_) => ComposerGraphCard::Switch,
                GraphNode::Join(_) => ComposerGraphCard::Join,
            };
            nodes.push(ComposerGraphVisualNode {
                id: node.id().to_owned(),
                scope: scope.clone(),
                card,
            });
        }
        for edge in &graph.edges {
            edges.push(ComposerGraphVisualEdge {
                from: edge.from.node.clone(),
                to: edge.to.node.clone(),
                kind: ComposerGraphEdgeKind::Flow,
                port: Some(edge.from.port.clone()),
            });
        }
        if let Some(scope) = &scope {
            for entry in &graph.entries {
                edges.push(ComposerGraphVisualEdge {
                    from: scope.owner_id().to_owned(),
                    to: entry.clone(),
                    kind: match scope {
                        ComposerGraphNestedScope::ForEachBody { .. } => {
                            ComposerGraphEdgeKind::Iteration
                        }
                        ComposerGraphNestedScope::IfThen { .. } => ComposerGraphEdgeKind::Then,
                        ComposerGraphNestedScope::IfElse { .. } => ComposerGraphEdgeKind::Else,
                        ComposerGraphNestedScope::SwitchCase { .. } => ComposerGraphEdgeKind::Case,
                        ComposerGraphNestedScope::SwitchDefault { .. } => {
                            ComposerGraphEdgeKind::Default
                        }
                    },
                    port: None,
                });
            }
        }
        for node in &graph.nodes {
            match node {
                GraphNode::ForEach(node) => {
                    visit(
                        &node.body,
                        Some(ComposerGraphNestedScope::ForEachBody {
                            owner_id: node.id.clone(),
                        }),
                        nodes,
                        edges,
                    );
                }
                GraphNode::If(node) => {
                    visit(
                        &node.then_graph,
                        Some(ComposerGraphNestedScope::IfThen {
                            owner_id: node.id.clone(),
                        }),
                        nodes,
                        edges,
                    );
                    if let Some(graph) = &node.else_graph {
                        visit(
                            graph,
                            Some(ComposerGraphNestedScope::IfElse {
                                owner_id: node.id.clone(),
                            }),
                            nodes,
                            edges,
                        );
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        visit(
                            &case.graph,
                            Some(ComposerGraphNestedScope::SwitchCase {
                                owner_id: node.id.clone(),
                                case_id: case.id.clone(),
                            }),
                            nodes,
                            edges,
                        );
                    }
                    if let Some(graph) = &node.default {
                        visit(
                            graph,
                            Some(ComposerGraphNestedScope::SwitchDefault {
                                owner_id: node.id.clone(),
                            }),
                            nodes,
                            edges,
                        );
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
    }

    let mut nodes = Vec::new();
    let mut edges = graph
        .entries
        .iter()
        .map(|entry| ComposerGraphVisualEdge {
            from: "start".into(),
            to: entry.clone(),
            kind: ComposerGraphEdgeKind::Flow,
            port: None,
        })
        .collect::<Vec<_>>();
    visit(graph, None, &mut nodes, &mut edges);
    (nodes, edges)
}

fn default_graph_canvas(graph: &WorkflowGraph) -> ComposerCanvas {
    let (nodes, edges) = graph_visual_model(graph);
    let mut canvas = ComposerCanvas::default();
    canvas
        .positions
        .insert("start".into(), CanvasPoint { x: 80.0, y: 250.0 });
    for (index, node) in nodes.iter().enumerate() {
        let parent_id = edges
            .iter()
            .find(|edge| edge.to == node.id)
            .map(|edge| edge.from.as_str())
            .unwrap_or("start");
        let parent = canvas
            .positions
            .get(parent_id)
            .copied()
            .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
        let iteration = edges
            .iter()
            .any(|edge| edge.to == node.id && edge.kind == ComposerGraphEdgeKind::Iteration);
        canvas.positions.insert(
            node.id.clone(),
            CanvasPoint {
                x: parent.x + 286.0,
                y: parent.y
                    + if iteration {
                        158.0
                    } else {
                        branch_offset(index)
                    },
            },
        );
    }
    canvas
}

fn graph_action_output_array(
    graph: &WorkflowGraph,
    node_id: &str,
) -> Option<(Binding, String, ContextType)> {
    let GraphNode::Action(node) = graph_node(graph, node_id)? else {
        return None;
    };
    let definition = definition_for_action(&node.step.action);
    let mut arrays = Vec::new();
    collect_schema_arrays(&definition.output_schema, "", &mut arrays);
    let (path, item_type) = arrays.into_iter().next()?;
    let mut field = FieldRef::step(node_id);
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        field = field.field(segment);
    }
    Some((
        Binding::field(field),
        item_alias_for_array_path(&path),
        item_type,
    ))
}

fn graph_loop_item_type(graph: &WorkflowGraph, loop_id: &str) -> Option<ContextType> {
    let GraphNode::ForEach(node) = graph_node(graph, loop_id)? else {
        return None;
    };
    let Binding::Field { field } = &node.collection else {
        return None;
    };
    match graph_field_context_type(graph, field, 0)? {
        ContextType::Array { items } => Some(*items),
        _ => None,
    }
}

fn graph_field_context_type(
    graph: &WorkflowGraph,
    field: &FieldRef,
    depth: usize,
) -> Option<ContextType> {
    if depth > 32 {
        return None;
    }
    let root = match &field.scope {
        ContextScope::Step { step_id } => {
            let GraphNode::Action(source) = graph_node(graph, step_id)? else {
                return None;
            };
            ContextType::object(definition_for_action(&source.step.action).output_schema)
        }
        ContextScope::LoopItem { step_id } => {
            graph_loop_item_type_at_depth(graph, step_id, depth + 1)?
        }
        ContextScope::Scenario => return None,
    };
    context_type_at_path(&root, &field.segments)
}

fn graph_loop_item_type_at_depth(
    graph: &WorkflowGraph,
    loop_id: &str,
    depth: usize,
) -> Option<ContextType> {
    let GraphNode::ForEach(node) = graph_node(graph, loop_id)? else {
        return None;
    };
    let Binding::Field { field } = &node.collection else {
        return None;
    };
    match graph_field_context_type(graph, field, depth)? {
        ContextType::Array { items } => Some(*items),
        _ => None,
    }
}

fn context_type_at_path(
    root: &ContextType,
    segments: &[ContextPathSegment],
) -> Option<ContextType> {
    if segments.is_empty() {
        return Some(root.clone());
    }
    match (root, &segments[0]) {
        (ContextType::Object { schema }, _) => schema
            .resolve_owned(segments)
            .map(|resolved| resolved.value_type),
        (ContextType::Array { items }, ContextPathSegment::Index { .. }) => {
            context_type_at_path(items, &segments[1..])
        }
        _ => None,
    }
}

fn loop_item_field(item_type: &ContextType, loop_id: &str, name: &str) -> Option<FieldRef> {
    context_type_at_path(item_type, &[ContextPathSegment::field(name)])?;
    Some(FieldRef::loop_item(loop_id).field(name))
}

/// Git blocks created inside a repository loop must consume the current item,
/// not the same prototype destination on every iteration. Keep these defaults
/// structural so renames and graph validation can still follow the references.
fn apply_loop_git_bindings(
    step: &Step,
    bindings: &mut BTreeMap<String, Binding>,
    loop_id: &str,
    item_type: &ContextType,
) {
    if !matches!(
        step.action.kind(),
        ActionKind::GitClone
            | ActionKind::GitInspect
            | ActionKind::GitCloneIfMissing
            | ActionKind::GitFetch
            | ActionKind::GitFastForward
    ) {
        return;
    }

    if let Some(repo) = loop_item_field(item_type, loop_id, "https_url")
        .or_else(|| loop_item_field(item_type, loop_id, "ssh_url"))
    {
        bindings.insert("repo".into(), Binding::field(repo));
    }

    let destination = if let Some(full_name) = loop_item_field(item_type, loop_id, "full_name") {
        Some(Binding::interpolated([
            TemplatePart::literal("$HOME/Developer/"),
            TemplatePart::field(full_name),
        ]))
    } else {
        loop_item_field(item_type, loop_id, "owner").and_then(|owner| {
            loop_item_field(item_type, loop_id, "name").map(|name| {
                Binding::interpolated([
                    TemplatePart::literal("$HOME/Developer/"),
                    TemplatePart::field(owner),
                    TemplatePart::literal("/"),
                    TemplatePart::field(name),
                ])
            })
        })
    };
    if let Some(destination) = destination {
        bindings.insert("dest".into(), destination);
    }

    if let Some(expected) = definition_for_action(&step.action)
        .input_schema
        .field("branch")
    {
        if let Some(field) = unique_exact_loop_binding_field(item_type, expected)
            .filter(|field| field.path == "default_branch")
        {
            bindings.insert(
                "branch".into(),
                Binding::field(FieldRef::loop_item(loop_id).field(field.path)),
            );
        }
    }
}

fn graph_make_node(
    graph: &WorkflowGraph,
    kind: ComposerGraphBlockKind,
    id: String,
    parent_id: Option<&str>,
    owner_loop: Option<&str>,
) -> GraphNode {
    if matches!(kind, ComposerGraphBlockKind::ForEach) {
        let (collection, item_alias_base, _) = parent_id
            .and_then(|parent| graph_action_output_array(graph, parent))
            .unwrap_or_else(|| {
                (
                    Binding::literal(serde_json::json!([])),
                    "item".into(),
                    ContextType::Any,
                )
            });
        let item_alias = first_free_loop_alias(graph, &item_alias_base, None);
        let index_alias = first_free_loop_alias(graph, &format!("{item_alias}_index"), None);
        return GraphNode::ForEach(ForEachNode {
            id,
            collection,
            item_alias,
            index_alias: Some(index_alias),
            concurrency: 1,
            on_error: LoopFailurePolicy::Stop,
            body: Box::new(WorkflowGraph::default()),
        });
    }

    if matches!(kind, ComposerGraphBlockKind::If) {
        return GraphNode::If(IfNode {
            id,
            condition: ExpressionV1::Literal {
                value: ExpressionValue::Bool(true),
            },
            then_graph: Box::new(WorkflowGraph::default()),
            else_graph: None,
        });
    }
    if matches!(kind, ComposerGraphBlockKind::Switch) {
        return GraphNode::Switch(SwitchNode {
            id,
            selector: Binding::literal(""),
            cases: vec![SwitchCase {
                id: "case-1".into(),
                values: vec![serde_json::json!("value")],
                graph: Box::new(WorkflowGraph::default()),
            }],
            default: None,
        });
    }
    if matches!(kind, ComposerGraphBlockKind::Join) {
        return GraphNode::Join(JoinNode {
            id,
            mode: JoinMode::All,
        });
    }

    let ComposerGraphBlockKind::Action(kind) = kind else {
        unreachable!("control nodes handled above")
    };
    let step = default_step(kind, id).expect("palette contains only graph action kinds");
    let mut bindings = BTreeMap::new();
    if let Some(loop_id) = owner_loop {
        if let Some(item_type) = graph_loop_item_type(graph, loop_id) {
            apply_loop_git_bindings(&step, &mut bindings, loop_id, &item_type);
        }
    }
    GraphNode::Action(Box::new(ActionNode { step, bindings }))
}

/// Conservative convenience binding for a freshly-created loop child.
///
/// A plain string input must never consume a semantically refined repository
/// name, URL, or branch merely because it is technically assignable. Automatic
/// selection is safe only when exactly one field has the same declared type
/// and satisfies the input's presence, nullability, and sensitivity contract.
fn unique_exact_loop_binding_field(
    item_type: &ContextType,
    expected: &FieldSchema,
) -> Option<ComposerBindableField> {
    let mut fields = Vec::new();
    collect_bindable_field_details(item_type, "", true, false, Sensitivity::Public, &mut fields);
    let mut compatible = fields.into_iter().filter(|field| {
        expected.value_type == field.value_type
            && (!expected.required || field.required)
            && (expected.nullable || !field.nullable)
            && (!field.sensitivity.is_secret() || expected.sensitivity.is_secret())
    });
    let field = compatible.next()?;
    compatible.next().is_none().then_some(field)
}

fn graph_loop_aliases(graph: &WorkflowGraph, exclude_node: Option<&str>) -> BTreeSet<String> {
    fn visit(graph: &WorkflowGraph, exclude_node: Option<&str>, aliases: &mut BTreeSet<String>) {
        for node in &graph.nodes {
            match node {
                GraphNode::ForEach(node) => {
                    if Some(node.id.as_str()) != exclude_node {
                        aliases.insert(node.item_alias.clone());
                        if let Some(alias) = &node.index_alias {
                            aliases.insert(alias.clone());
                        }
                    }
                    visit(&node.body, exclude_node, aliases);
                }
                GraphNode::If(node) => {
                    visit(&node.then_graph, exclude_node, aliases);
                    if let Some(graph) = &node.else_graph {
                        visit(graph, exclude_node, aliases);
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        visit(&case.graph, exclude_node, aliases);
                    }
                    if let Some(graph) = &node.default {
                        visit(graph, exclude_node, aliases);
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
    }

    let mut aliases = BTreeSet::new();
    visit(graph, exclude_node, &mut aliases);
    aliases
}

fn first_free_loop_alias(graph: &WorkflowGraph, base: &str, exclude_node: Option<&str>) -> String {
    let used = graph_loop_aliases(graph, exclude_node);
    if !used.contains(base) {
        return base.into();
    }
    (2usize..)
        .map(|ordinal| format!("{base}_{ordinal}"))
        .find(|candidate| !used.contains(candidate))
        .expect("unbounded loop alias sequence has a free value")
}

fn graph_insert_composer_block(
    graph: &mut WorkflowGraph,
    attach: &ComposerGraphAttach,
    source_port: Option<EdgePort>,
    kind: ComposerGraphBlockKind,
) -> Result<String, String> {
    if let Some(message) = graph_attach_blocker(graph, attach) {
        return Err(message);
    }
    let base_id = match kind {
        ComposerGraphBlockKind::Action(kind) => kind.id(),
        ComposerGraphBlockKind::ForEach => "for-each",
        ComposerGraphBlockKind::If => "if",
        ComposerGraphBlockKind::Switch => "switch",
        ComposerGraphBlockKind::Join => "join",
    };
    let ids = graph_node_ids(graph);
    let mut suffix = ids.len() + 1;
    let id = loop {
        let candidate = format!("{base_id}-{suffix}");
        if !ids.contains(&candidate) {
            break candidate;
        }
        suffix += 1;
    };
    let (parent_id, owner_loop) = match attach {
        ComposerGraphAttach::RootStart => (None, None),
        ComposerGraphAttach::RootAfter { node_id } => (Some(node_id.as_str()), None),
        ComposerGraphAttach::NestedStart { scope } => (
            None,
            match scope {
                ComposerGraphNestedScope::ForEachBody { owner_id } => Some(owner_id.as_str()),
                _ => None,
            },
        ),
        ComposerGraphAttach::NestedAfter { scope, node_id } => (
            Some(node_id.as_str()),
            match scope {
                ComposerGraphNestedScope::ForEachBody { owner_id } => Some(owner_id.as_str()),
                _ => None,
            },
        ),
    };
    let incoming = match attach {
        ComposerGraphAttach::RootStart | ComposerGraphAttach::NestedStart { .. } => None,
        ComposerGraphAttach::RootAfter { node_id } => {
            let source = graph
                .nodes
                .iter()
                .find(|node| node.id() == node_id)
                .ok_or_else(|| format!("родительский блок {node_id} не найден"))?;
            let available_ports = graph_node_output_ports(source);
            let port = source_port.unwrap_or_else(|| graph_node_flow_port(source));
            if !available_ports.contains(&port) {
                return Err(format!(
                    "порт {} недоступен для блока {node_id}",
                    graph_edge_port_label(&port)
                ));
            }
            Some((node_id.clone(), port))
        }
        ComposerGraphAttach::NestedAfter { scope, node_id } => {
            let target = graph_nested_scope(graph, scope)
                .ok_or_else(|| format!("вложенная область {} не найдена", scope.owner_id()))?;
            let source = target
                .nodes
                .iter()
                .find(|node| node.id() == node_id)
                .ok_or_else(|| format!("родительский блок {node_id} не найден"))?;
            let available_ports = graph_node_output_ports(source);
            let port = source_port.unwrap_or_else(|| graph_node_flow_port(source));
            if !available_ports.contains(&port) {
                return Err(format!(
                    "порт {} недоступен для блока {node_id}",
                    graph_edge_port_label(&port)
                ));
            }
            Some((node_id.clone(), port))
        }
    };
    let node = graph_make_node(graph, kind, id.clone(), parent_id, owner_loop);
    let target = match attach {
        ComposerGraphAttach::RootStart | ComposerGraphAttach::RootAfter { .. } => graph,
        ComposerGraphAttach::NestedStart { scope }
        | ComposerGraphAttach::NestedAfter { scope, .. } => graph_nested_scope_mut(graph, scope)
            .ok_or_else(|| format!("вложенная область {} не найдена", scope.owner_id()))?,
    };
    target.nodes.push(node);
    match attach {
        ComposerGraphAttach::RootStart | ComposerGraphAttach::NestedStart { .. } => {
            target.entries.push(id.clone());
        }
        ComposerGraphAttach::RootAfter { .. } | ComposerGraphAttach::NestedAfter { .. } => {
            let (source_id, port) = incoming.expect("after attachment has an incoming edge");
            target.edges.push(GraphEdge::new(source_id, port, &id));
        }
    }
    Ok(id)
}

fn graph_node_flow_port(node: &GraphNode) -> EdgePort {
    match node {
        GraphNode::Action(_) => EdgePort::Success,
        GraphNode::ForEach(_) | GraphNode::If(_) | GraphNode::Switch(_) | GraphNode::Join(_) => {
            EdgePort::Completed
        }
    }
}

fn graph_node_output_ports(node: &GraphNode) -> Vec<EdgePort> {
    match node {
        GraphNode::Action(_) => vec![EdgePort::Success, EdgePort::Failure, EdgePort::Always],
        GraphNode::ForEach(_) => {
            vec![EdgePort::Completed, EdgePort::Empty, EdgePort::Failure]
        }
        GraphNode::If(_) | GraphNode::Switch(_) | GraphNode::Join(_) => {
            vec![EdgePort::Completed, EdgePort::Failure]
        }
    }
}

fn graph_attach_blocker(graph: &WorkflowGraph, attach: &ComposerGraphAttach) -> Option<String> {
    let source_id = match attach {
        ComposerGraphAttach::RootAfter { node_id }
        | ComposerGraphAttach::NestedAfter { node_id, .. } => node_id,
        ComposerGraphAttach::RootStart | ComposerGraphAttach::NestedStart { .. } => return None,
    };
    match graph_node(graph, source_id)? {
        GraphNode::ForEach(node) if node.body.nodes.is_empty() => Some(format!(
            "Цикл «{}» пока пуст. Сначала добавьте блок через «＋ Для каждого item»; выход «После цикла» станет доступен после этого.",
            node.item_alias
        )),
        GraphNode::If(node) => {
            let mut missing = Vec::new();
            if node.then_graph.nodes.is_empty() {
                missing.push("«Тогда»");
            }
            if node
                .else_graph
                .as_deref()
                .is_some_and(|graph| graph.nodes.is_empty())
            {
                missing.push("«Иначе»");
            }
            (!missing.is_empty()).then(|| {
                format!(
                    "У блока «Если / иначе» не заполнены ветки: {}. Сначала добавьте действие в каждую указанную ветку; выход «После условия» станет доступен после этого.",
                    missing.join(", ")
                )
            })
        }
        GraphNode::Switch(node) => {
            let mut missing = node
                .cases
                .iter()
                .filter(|case| case.graph.nodes.is_empty())
                .map(|case| format!("вариант «{}»", case.id))
                .collect::<Vec<_>>();
            if node
                .default
                .as_deref()
                .is_some_and(|graph| graph.nodes.is_empty())
            {
                missing.push("ветка «По умолчанию»".into());
            }
            (!missing.is_empty()).then(|| {
                format!(
                    "У блока «Выбор по значению» не заполнены ветки: {}. Сначала добавьте действие в каждую указанную ветку; выход «После выбора» станет доступен после этого.",
                    missing.join(", ")
                )
            })
        }
        GraphNode::Join(_) => {
            let scope = graph_local_scope(graph, source_id)?;
            let incoming = scope
                .edges
                .iter()
                .filter(|edge| {
                    edge.to.node == source_id.as_str()
                        && matches!(edge.to.port, EdgePort::Input)
                        && scope.nodes.iter().any(|candidate| {
                            candidate.id() == edge.from.node
                                && graph_node_output_ports(candidate).contains(&edge.from.port)
                        })
                })
                .count();
            (incoming < 2).then(|| {
                format!(
                    "У блока «Объединение ветвей» только {incoming} корректных входящих связей. Подключите минимум две ветки перед добавлением следующего блока."
                )
            })
        }
        GraphNode::Action(_) | GraphNode::ForEach(_) => None,
    }
}

fn graph_node_label(graph: &WorkflowGraph, node_id: &str) -> String {
    match graph_node(graph, node_id) {
        Some(GraphNode::Action(node)) => node.step.name.clone(),
        Some(GraphNode::ForEach(node)) => format!("Для каждого {}", node.item_alias),
        Some(GraphNode::If(_)) => "Если / иначе".into(),
        Some(GraphNode::Switch(_)) => "Выбор по значению".into(),
        Some(GraphNode::Join(_)) => "Объединение ветвей".into(),
        None => node_id.to_owned(),
    }
}

fn graph_owner_id_from_path(path: &str) -> Option<&str> {
    let start = path.rfind("node[")? + "node[".len();
    let rest = &path[start..];
    let end = rest.find(']')?;
    Some(&rest[..end])
}

fn graph_switch_case_id_from_path(graph: &WorkflowGraph, path: &str) -> Option<String> {
    let owner = graph_owner_id_from_path(path)?;
    let start = path.rfind(".cases[")? + ".cases[".len();
    let index = path[start..].split_once(']')?.0.parse::<usize>().ok()?;
    let GraphNode::Switch(node) = graph_node(graph, owner)? else {
        return None;
    };
    node.cases.get(index).map(|case| case.id.clone())
}

fn graph_validation_message(graph: &WorkflowGraph, error: &GraphValidationError) -> String {
    use GraphValidationErrorKind as Kind;

    let node_label = |node: &str| graph_node_label(graph, node);
    match &error.kind {
        Kind::EmptyGraph | Kind::EmptyEntries if error.path.ends_with(".body") => {
            let owner = graph_owner_id_from_path(&error.path).unwrap_or("цикл");
            format!(
                "Цикл «{}» пуст. Добавьте действие через «＋ Для каждого item».",
                node_label(owner)
            )
        }
        Kind::EmptyGraph | Kind::EmptyEntries if error.path.ends_with(".then") => {
            let owner = graph_owner_id_from_path(&error.path).unwrap_or("условие");
            format!(
                "Ветка «Тогда» блока «{}» пуста. Добавьте в неё действие.",
                node_label(owner)
            )
        }
        Kind::EmptyGraph | Kind::EmptyEntries if error.path.ends_with(".else") => {
            let owner = graph_owner_id_from_path(&error.path).unwrap_or("условие");
            format!(
                "Ветка «Иначе» блока «{}» пуста. Добавьте в неё действие или удалите необязательную ветку.",
                node_label(owner)
            )
        }
        Kind::EmptyGraph | Kind::EmptyEntries
            if error.path.contains(".cases[") && error.path.ends_with("].graph") =>
        {
            let owner = graph_owner_id_from_path(&error.path).unwrap_or("выбор");
            let case = graph_switch_case_id_from_path(graph, &error.path)
                .unwrap_or_else(|| "неизвестный".into());
            format!(
                "Вариант «{case}» блока «{}» пуст. Добавьте в него действие.",
                node_label(owner)
            )
        }
        Kind::EmptyGraph | Kind::EmptyEntries if error.path.ends_with(".default") => {
            let owner = graph_owner_id_from_path(&error.path).unwrap_or("выбор");
            format!(
                "Ветка «По умолчанию» блока «{}» пуста. Добавьте в неё действие или удалите необязательную ветку.",
                node_label(owner)
            )
        }
        Kind::EmptyGraph => "В сценарии пока нет блоков.".into(),
        Kind::EmptyEntries => {
            "Нет начального блока: соедините первый блок с «Началом сценария».".into()
        }
        Kind::InvalidAction { node, .. }
            if matches!(
                graph_node(graph, node),
                Some(GraphNode::Action(action))
                    if matches!(action.step.action, Action::GithubListRepositories)
            ) =>
        {
            format!(
                "У блока «{}» недопустимая политика безопасности. Для него нужны: аутентификация — «Нет», повышение прав — «Запрещено», опасная операция — выключена.",
                node_label(node)
            )
        }
        Kind::InvalidAction { node, .. } => format!(
            "Проверьте обязательные параметры и политики блока «{}».",
            node_label(node)
        ),
        Kind::ForEachCollectionNotArray { node, .. } => format!(
            "В блоке «{}» выбрано не поле-массив. Выберите коллекцию с пометкой [].",
            node_label(node)
        ),
        Kind::InvalidConcurrency { node } => format!(
            "В блоке «{}» параллельность должна быть от 1 до 64.",
            node_label(node)
        ),
        Kind::InvalidAlias { node, .. }
        | Kind::ShadowedAlias { node, .. }
        | Kind::DuplicateAlias { node, .. } => format!(
            "В блоке «{}» задайте уникальное корректное имя текущего элемента.",
            node_label(node)
        ),
        Kind::UnknownBindingField { node, field } => format!(
            "У блока «{}» нет входа «{field}». Выберите другой вход или удалите старую привязку.",
            node_label(node)
        ),
        Kind::BindingTypeMismatch { node, field, .. } => format!(
            "Поле «{field}» блока «{}» получает значение несовместимого типа.",
            node_label(node)
        ),
        Kind::BindingMayBeMissing { node, field } => format!(
            "Поле «{field}» блока «{}» связано с необязательным значением.",
            node_label(node)
        ),
        Kind::BindingMayBeNull { node, field } => format!(
            "Поле «{field}» блока «{}» не принимает null.",
            node_label(node)
        ),
        Kind::InvalidBindingValue { node, field, .. } => format!(
            "Некорректное значение входа «{field}» в блоке «{}».",
            node_label(node)
        ),
        Kind::SecretBindingFlow { node, field } => format!(
            "Секрет нельзя передать в публичный вход «{field}» блока «{}».",
            node_label(node)
        ),
        Kind::UnknownContextField {
            consumer,
            producer,
            field,
        } => format!(
            "Блок «{}» запрашивает отсутствующее поле «{field}» у блока «{}».",
            node_label(consumer),
            node_label(producer)
        ),
        Kind::ContextNotVisible { consumer, producer }
        | Kind::UnknownContextStep { consumer, producer } => format!(
            "Контекст блока «{}» недоступен блоку «{}» в этой ветви.",
            node_label(producer),
            node_label(consumer)
        ),
        Kind::LoopContextNotVisible {
            consumer,
            loop_node,
        } => format!(
            "Текущий item цикла «{}» доступен только блокам внутри цикла; сейчас его использует «{}».",
            node_label(loop_node),
            node_label(consumer)
        ),
        Kind::UnreachableNode { node } => format!(
            "Блок «{}» не соединён с началом или предыдущим блоком.",
            node_label(node)
        ),
        Kind::UnknownEntry { node } | Kind::UnknownEndpoint { node } => {
            format!("Связь указывает на удалённый блок «{node}».")
        }
        Kind::Cycle { .. } => "Связи образуют бесконечный цикл; удалите обратную связь.".into(),
        Kind::JoinNeedsMultipleInputs { node, .. } => format!(
            "Блоку «{}» нужны как минимум две входящие ветви.",
            node_label(node)
        ),
        _ => format!(
            "Проверьте структуру, входы и связи в области «{}».",
            error.path
        ),
    }
}

fn validate_graph_for_ui(task: &Task, graph: &WorkflowGraph) -> Result<(), String> {
    graph.validate().map_err(|errors| {
        let mut messages = Vec::new();
        for error in errors {
            let message = graph_validation_message(graph, &error);
            if !messages.contains(&message) {
                messages.push(message);
            }
        }
        format!(
            "Сценарий «{}» пока не готов:\n• {}",
            task.name,
            messages.join("\n• ")
        )
    })
}

fn graph_edge_port_label(port: &EdgePort) -> &'static str {
    match port {
        EdgePort::Input => "вход",
        EdgePort::Success => "успех",
        EdgePort::Failure => "ошибка",
        EdgePort::Always => "всегда",
        EdgePort::Completed => "после цикла",
        EdgePort::Empty => "пустая коллекция",
    }
}

fn graph_reachable_nodes(graph: &WorkflowGraph, ignored_node: Option<&str>) -> BTreeSet<String> {
    let mut reachable = graph
        .entries
        .iter()
        .filter(|entry| Some(entry.as_str()) != ignored_node)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut pending = reachable.iter().cloned().collect::<Vec<_>>();
    while let Some(source) = pending.pop() {
        for target in graph
            .edges
            .iter()
            .filter(|edge| {
                edge.from.node == source
                    && Some(edge.from.node.as_str()) != ignored_node
                    && Some(edge.to.node.as_str()) != ignored_node
            })
            .map(|edge| edge.to.node.clone())
        {
            if reachable.insert(target.clone()) {
                pending.push(target);
            }
        }
    }
    reachable
}

fn graph_removal_blockers(graph: &WorkflowGraph, id: &str) -> Vec<String> {
    let before = graph_reachable_nodes(graph, None);
    let after = graph_reachable_nodes(graph, Some(id));
    before
        .difference(&after)
        .filter(|node_id| node_id.as_str() != id)
        .cloned()
        .collect()
}

fn binding_references_graph_node(binding: &Binding, node_id: &str) -> bool {
    let matches = |field: &FieldRef| match &field.scope {
        ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => step_id == node_id,
        ContextScope::Scenario => false,
    };
    match binding {
        Binding::Field { field } => matches(field),
        Binding::Interpolated { parts } => parts
            .iter()
            .any(|part| matches!(part, TemplatePart::Field { field } if matches(field))),
        Binding::Literal { .. } | Binding::Template { .. } => false,
    }
}

fn expression_references_graph_node(expression: &ExpressionV1, node_id: &str) -> bool {
    let mut found = false;
    expression.visit_context_references(|field| {
        found |= match &field.scope {
            ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                step_id == node_id
            }
            ContextScope::Scenario => false,
        };
    });
    found
}

fn step_condition_references_graph_node(condition: &StepCondition, node_id: &str) -> bool {
    match condition {
        StepCondition::ExitCode { step, .. } => step == node_id,
        StepCondition::Expression { rule, .. } => expression_references_graph_node(rule, node_id),
        StepCondition::All { conditions } | StepCondition::Any { conditions } => conditions
            .iter()
            .any(|condition| step_condition_references_graph_node(condition, node_id)),
        StepCondition::Not { condition } => {
            step_condition_references_graph_node(condition, node_id)
        }
        StepCondition::Path { .. } => false,
    }
}

fn graph_node_reference_users(graph: &WorkflowGraph, node_id: &str) -> Vec<String> {
    fn visit(graph: &WorkflowGraph, removed_id: &str, users: &mut BTreeSet<String>) {
        for node in &graph.nodes {
            if node.id() == removed_id {
                // Nested scopes owned by the removed control node disappear
                // with it, so references inside that same subtree are not
                // external users. Non-empty scopes are blocked separately.
                continue;
            }
            let referenced = match node {
                GraphNode::Action(node) => {
                    node.bindings
                        .values()
                        .chain(node.step.bindings.values())
                        .any(|binding| binding_references_graph_node(binding, removed_id))
                        || [node.step.when.as_ref(), node.step.require.as_ref()]
                            .into_iter()
                            .flatten()
                            .any(|condition| {
                                step_condition_references_graph_node(condition, removed_id)
                            })
                }
                GraphNode::ForEach(node) => {
                    binding_references_graph_node(&node.collection, removed_id)
                }
                GraphNode::If(node) => {
                    expression_references_graph_node(&node.condition, removed_id)
                }
                GraphNode::Switch(node) => {
                    binding_references_graph_node(&node.selector, removed_id)
                }
                GraphNode::Join(_) => false,
            };
            if referenced {
                users.insert(node.id().to_owned());
            }
            match node {
                GraphNode::ForEach(node) => visit(&node.body, removed_id, users),
                GraphNode::If(node) => {
                    visit(&node.then_graph, removed_id, users);
                    if let Some(graph) = &node.else_graph {
                        visit(graph, removed_id, users);
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        visit(&case.graph, removed_id, users);
                    }
                    if let Some(graph) = &node.default {
                        visit(graph, removed_id, users);
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
    }

    let mut users = BTreeSet::new();
    visit(graph, node_id, &mut users);
    users.into_iter().collect()
}

fn graph_node_has_nested_content(node: &GraphNode) -> bool {
    match node {
        GraphNode::ForEach(node) => !graph_scope_is_empty(&node.body),
        GraphNode::If(node) => {
            !graph_scope_is_empty(&node.then_graph)
                || node
                    .else_graph
                    .as_deref()
                    .is_some_and(|graph| !graph_scope_is_empty(graph))
        }
        GraphNode::Switch(node) => {
            node.cases
                .iter()
                .any(|case| !graph_scope_is_empty(&case.graph))
                || node
                    .default
                    .as_deref()
                    .is_some_and(|graph| !graph_scope_is_empty(graph))
        }
        GraphNode::Action(_) | GraphNode::Join(_) => false,
    }
}

fn graph_scope_is_empty(graph: &WorkflowGraph) -> bool {
    graph.nodes.is_empty()
        && graph.edges.is_empty()
        && graph.entries.is_empty()
        && graph.exits.is_empty()
}

fn graph_remove_optional_scope(
    graph: &mut WorkflowGraph,
    scope: &ComposerGraphNestedScope,
) -> Result<bool, String> {
    let (label, branch) = match scope {
        ComposerGraphNestedScope::IfElse { owner_id } => {
            let GraphNode::If(node) =
                graph_node(graph, owner_id).ok_or_else(|| format!("узел {owner_id} не найден"))?
            else {
                return Err(format!("узел {owner_id} не является IF"));
            };
            ("Иначе", node.else_graph.as_deref())
        }
        ComposerGraphNestedScope::SwitchDefault { owner_id } => {
            let GraphNode::Switch(node) =
                graph_node(graph, owner_id).ok_or_else(|| format!("узел {owner_id} не найден"))?
            else {
                return Err(format!("узел {owner_id} не является SWITCH"));
            };
            ("По умолчанию", node.default.as_deref())
        }
        _ => return Err("Эту обязательную ветку нельзя удалить.".into()),
    };
    let Some(branch) = branch else {
        return Ok(false);
    };
    if !graph_scope_is_empty(branch) {
        return Err(format!(
            "Ветка «{label}» не пуста. Сначала удалите или перенесите блоки из неё."
        ));
    }

    match scope {
        ComposerGraphNestedScope::IfElse { owner_id } => {
            let GraphNode::If(node) = graph_node_mut(graph, owner_id)
                .ok_or_else(|| format!("узел {owner_id} не найден"))?
            else {
                return Err(format!("узел {owner_id} не является IF"));
            };
            node.else_graph = None;
        }
        ComposerGraphNestedScope::SwitchDefault { owner_id } => {
            let GraphNode::Switch(node) = graph_node_mut(graph, owner_id)
                .ok_or_else(|| format!("узел {owner_id} не найден"))?
            else {
                return Err(format!("узел {owner_id} не является SWITCH"));
            };
            node.default = None;
        }
        _ => unreachable!("optional scope kind checked above"),
    }
    Ok(true)
}

fn graph_remove_switch_case(
    graph: &mut WorkflowGraph,
    switch_id: &str,
    case_id: &str,
) -> Result<bool, String> {
    let GraphNode::Switch(node) =
        graph_node(graph, switch_id).ok_or_else(|| format!("узел {switch_id} не найден"))?
    else {
        return Err(format!("узел {switch_id} не является SWITCH"));
    };
    let Some(case) = node.cases.iter().find(|case| case.id == case_id) else {
        return Ok(false);
    };
    if node.cases.len() == 1 {
        return Err("SWITCH должен содержать хотя бы один case.".into());
    }
    if !graph_scope_is_empty(&case.graph) {
        return Err(format!(
            "Вариант «{case_id}» не пуст. Сначала удалите или перенесите блоки из него."
        ));
    }

    let GraphNode::Switch(node) =
        graph_node_mut(graph, switch_id).ok_or_else(|| format!("узел {switch_id} не найден"))?
    else {
        return Err(format!("узел {switch_id} не является SWITCH"));
    };
    let Some(index) = node.cases.iter().position(|case| case.id == case_id) else {
        return Ok(false);
    };
    node.cases.remove(index);
    Ok(true)
}

fn graph_remove_composer_node(graph: &mut WorkflowGraph, id: &str) -> Result<bool, String> {
    if let Some(index) = graph.nodes.iter().position(|node| node.id() == id) {
        if graph_node_has_nested_content(&graph.nodes[index]) {
            return Err(
                "Сначала удалите или перенесите блоки из вложенных ветвей; обычное удаление не удаляет ветку каскадно."
                    .into(),
            );
        }
        let reference_users = graph_node_reference_users(graph, id);
        if !reference_users.is_empty() {
            return Err(format!(
                "Сначала удалите привязки и условия, которые ссылаются на этот блок: {}",
                reference_users.join(", ")
            ));
        }
        let blockers = graph_removal_blockers(graph, id);
        if !blockers.is_empty() {
            return Err(format!(
                "Сначала удалите или переподключите downstream-блоки: {}",
                blockers.join(", ")
            ));
        }
        graph.nodes.remove(index);
        graph
            .edges
            .retain(|edge| edge.from.node != id && edge.to.node != id);
        graph.entries.retain(|entry| entry != id);
        graph.exits.retain(|exit| exit.from.node != id);
        return Ok(true);
    }

    for node in &mut graph.nodes {
        let removed = match node {
            GraphNode::ForEach(node) => graph_remove_composer_node(&mut node.body, id),
            GraphNode::If(node) => {
                if graph_remove_composer_node(&mut node.then_graph, id)? {
                    Ok(true)
                } else if let Some(graph) = node.else_graph.as_deref_mut() {
                    graph_remove_composer_node(graph, id)
                } else {
                    Ok(false)
                }
            }
            GraphNode::Switch(node) => {
                let mut removed = false;
                for case in &mut node.cases {
                    if graph_remove_composer_node(&mut case.graph, id)? {
                        removed = true;
                        break;
                    }
                }
                if removed {
                    Ok(true)
                } else if let Some(graph) = node.default.as_deref_mut() {
                    graph_remove_composer_node(graph, id)
                } else {
                    Ok(false)
                }
            }
            GraphNode::Action(_) | GraphNode::Join(_) => Ok(false),
        };
        if removed? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn graph_local_dominators(graph: &WorkflowGraph, node_id: &str) -> BTreeSet<String> {
    let all = graph
        .nodes
        .iter()
        .map(|node| node.id().to_owned())
        .collect::<BTreeSet<_>>();
    if !all.contains(node_id) {
        return BTreeSet::new();
    }
    let entries = graph.entries.iter().cloned().collect::<BTreeSet<_>>();
    let mut dominators = all
        .iter()
        .map(|node| {
            let initial = if entries.contains(node) {
                BTreeSet::from([node.clone()])
            } else {
                all.clone()
            };
            (node.clone(), initial)
        })
        .collect::<BTreeMap<_, _>>();
    loop {
        let mut changed = false;
        for node in all.iter().filter(|node| !entries.contains(*node)) {
            let predecessors = graph
                .edges
                .iter()
                .filter(|edge| edge.to.node == *node)
                .map(|edge| edge.from.node.as_str())
                .collect::<Vec<_>>();
            let mut next = if let Some(first) = predecessors.first() {
                dominators.get(*first).cloned().unwrap_or_default()
            } else {
                BTreeSet::new()
            };
            for predecessor in predecessors.iter().skip(1) {
                let candidate = dominators.get(*predecessor).cloned().unwrap_or_default();
                next.retain(|id| candidate.contains(id));
            }
            next.insert(node.clone());
            if dominators.get(node) != Some(&next) {
                dominators.insert(node.clone(), next);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut result = dominators.remove(node_id).unwrap_or_default();
    result.remove(node_id);
    result
}

fn graph_binding_options(
    graph: &WorkflowGraph,
    consumer_id: &str,
    expected: &FieldSchema,
) -> Vec<ComposerGraphBindingOption> {
    fn push_action_sources(
        graph: &WorkflowGraph,
        ids: &BTreeSet<String>,
        output: &mut Vec<ComposerGraphBindingOption>,
    ) {
        for source_id in ids {
            let Some(GraphNode::Action(node)) =
                graph.nodes.iter().find(|node| node.id() == source_id)
            else {
                continue;
            };
            let definition = definition_for_action(&node.step.action);
            let root = ContextType::object(definition.output_schema);
            let mut fields = Vec::new();
            collect_bindable_field_details(
                &root,
                "",
                true,
                false,
                Sensitivity::Public,
                &mut fields,
            );
            for field in fields {
                let reference = field
                    .path
                    .split('.')
                    .filter(|segment| !segment.is_empty())
                    .fold(FieldRef::step(source_id), |reference, segment| {
                        reference.field(segment)
                    });
                output.push(ComposerGraphBindingOption {
                    label: format!(
                        "{}.{} · {}",
                        source_id,
                        field.path,
                        context_type_label(&field.value_type, field.nullable, !field.required)
                    ),
                    binding: Binding::field(reference),
                    value_type: field.value_type,
                    required: field.required,
                    nullable: field.nullable,
                    sensitivity: field.sensitivity,
                });
            }
        }
    }

    fn find_scope(
        root: &WorkflowGraph,
        graph: &WorkflowGraph,
        consumer_id: &str,
        inherited: &[ComposerGraphBindingOption],
    ) -> Option<Vec<ComposerGraphBindingOption>> {
        if graph.nodes.iter().any(|node| node.id() == consumer_id) {
            let mut output = inherited.to_vec();
            let ancestors = graph_local_dominators(graph, consumer_id);
            push_action_sources(graph, &ancestors, &mut output);
            return Some(output);
        }
        for node in &graph.nodes {
            match node {
                GraphNode::ForEach(node) => {
                    let mut nested = inherited.to_vec();
                    let outer_ancestors = graph_local_dominators(graph, &node.id);
                    push_action_sources(graph, &outer_ancestors, &mut nested);
                    if let Some(item_type) = graph_loop_item_type(root, &node.id) {
                        let mut fields = Vec::new();
                        collect_bindable_field_details(
                            &item_type,
                            "",
                            true,
                            false,
                            Sensitivity::Public,
                            &mut fields,
                        );
                        for field in fields {
                            let reference = field
                                .path
                                .split('.')
                                .filter(|segment| !segment.is_empty())
                                .fold(FieldRef::loop_item(&node.id), |reference, segment| {
                                    reference.field(segment)
                                });
                            nested.push(ComposerGraphBindingOption {
                                label: format!(
                                    "{} (item).{} · {}",
                                    node.item_alias,
                                    field.path,
                                    context_type_label(
                                        &field.value_type,
                                        field.nullable,
                                        !field.required
                                    )
                                ),
                                binding: Binding::field(reference),
                                value_type: field.value_type,
                                required: field.required,
                                nullable: field.nullable,
                                sensitivity: field.sensitivity,
                            });
                        }
                    }
                    if let Some(found) = find_scope(root, &node.body, consumer_id, &nested) {
                        return Some(found);
                    }
                }
                GraphNode::If(node) => {
                    let mut nested = inherited.to_vec();
                    let outer = graph_local_dominators(graph, &node.id);
                    push_action_sources(graph, &outer, &mut nested);
                    if let Some(found) = find_scope(root, &node.then_graph, consumer_id, &nested) {
                        return Some(found);
                    }
                    if let Some(found) = node
                        .else_graph
                        .as_deref()
                        .and_then(|graph| find_scope(root, graph, consumer_id, &nested))
                    {
                        return Some(found);
                    }
                }
                GraphNode::Switch(node) => {
                    let mut nested = inherited.to_vec();
                    let outer = graph_local_dominators(graph, &node.id);
                    push_action_sources(graph, &outer, &mut nested);
                    for case in &node.cases {
                        if let Some(found) = find_scope(root, &case.graph, consumer_id, &nested) {
                            return Some(found);
                        }
                    }
                    if let Some(found) = node
                        .default
                        .as_deref()
                        .and_then(|graph| find_scope(root, graph, consumer_id, &nested))
                    {
                        return Some(found);
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
        None
    }

    let mut options = find_scope(graph, graph, consumer_id, &[]).unwrap_or_default();
    options.retain(|option| {
        expected.value_type.is_assignable_from(&option.value_type)
            && (!expected.required || option.required)
            && (expected.nullable || !option.nullable)
            && (!option.sensitivity.is_secret() || expected.sensitivity.is_secret())
    });
    options.sort_by(|left, right| left.label.cmp(&right.label));
    options.dedup_by(|left, right| left.binding == right.binding);
    options
}

fn graph_condition_fields(graph: &WorkflowGraph, consumer_id: &str) -> Vec<ComposerConditionField> {
    let expected = FieldSchema::optional(ContextType::Any).nullable();
    graph_binding_options(graph, consumer_id, &expected)
        .into_iter()
        .filter_map(|option| {
            let Binding::Field { field } = option.binding else {
                return None;
            };
            Some(ComposerConditionField {
                reference: field,
                label: option.label,
                value_type: option.value_type,
                required: option.required,
                nullable: option.nullable,
            })
        })
        .collect()
}

fn graph_array_options(
    graph: &WorkflowGraph,
    consumer_id: &str,
) -> Vec<(String, Binding, String, ContextType)> {
    type ArrayOption = (String, Binding, String, ContextType);

    fn push_action_arrays(
        graph: &WorkflowGraph,
        source_ids: &BTreeSet<String>,
        output: &mut Vec<ArrayOption>,
    ) {
        for source_id in source_ids {
            let Some(GraphNode::Action(node)) =
                graph.nodes.iter().find(|node| node.id() == source_id)
            else {
                continue;
            };
            let definition = definition_for_action(&node.step.action);
            let mut arrays = Vec::new();
            collect_schema_arrays(&definition.output_schema, "", &mut arrays);
            for (path, item_type) in arrays {
                let reference = path
                    .split('.')
                    .filter(|segment| !segment.is_empty())
                    .fold(FieldRef::step(source_id), |reference, segment| {
                        reference.field(segment)
                    });
                output.push((
                    format!("{source_id}.{path}[]"),
                    Binding::field(reference),
                    item_alias_for_array_path(&path),
                    item_type,
                ));
            }
        }
    }

    fn push_loop_item_arrays(
        root: &WorkflowGraph,
        node: &ForEachNode,
        output: &mut Vec<ArrayOption>,
    ) {
        let Some(item_type) = graph_loop_item_type(root, &node.id) else {
            return;
        };
        let mut arrays = Vec::new();
        match item_type {
            ContextType::Array { items } => arrays.push((String::new(), *items)),
            ContextType::Object { schema } => collect_schema_arrays(&schema, "", &mut arrays),
            _ => return,
        }
        for (path, nested_item_type) in arrays {
            let reference = path
                .split('.')
                .filter(|segment| !segment.is_empty())
                .fold(FieldRef::loop_item(&node.id), |reference, segment| {
                    reference.field(segment)
                });
            let label = if path.is_empty() {
                format!("{} (item)[]", node.item_alias)
            } else {
                format!("{} (item).{path}[]", node.item_alias)
            };
            output.push((
                label,
                Binding::field(reference),
                item_alias_for_array_path(&path),
                nested_item_type,
            ));
        }
    }

    fn find_options(
        root: &WorkflowGraph,
        current: &WorkflowGraph,
        consumer_id: &str,
        inherited: &[ArrayOption],
    ) -> Option<Vec<ArrayOption>> {
        if current.nodes.iter().any(|node| node.id() == consumer_id) {
            let mut output = inherited.to_vec();
            push_action_arrays(
                current,
                &graph_local_dominators(current, consumer_id),
                &mut output,
            );
            return Some(output);
        }
        for node in &current.nodes {
            let mut nested = inherited.to_vec();
            push_action_arrays(
                current,
                &graph_local_dominators(current, node.id()),
                &mut nested,
            );
            match node {
                GraphNode::ForEach(node) => {
                    push_loop_item_arrays(root, node, &mut nested);
                    if let Some(found) = find_options(root, &node.body, consumer_id, &nested) {
                        return Some(found);
                    }
                }
                GraphNode::If(node) => {
                    if let Some(found) = find_options(root, &node.then_graph, consumer_id, &nested)
                    {
                        return Some(found);
                    }
                    if let Some(found) = node
                        .else_graph
                        .as_deref()
                        .and_then(|graph| find_options(root, graph, consumer_id, &nested))
                    {
                        return Some(found);
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        if let Some(found) = find_options(root, &case.graph, consumer_id, &nested) {
                            return Some(found);
                        }
                    }
                    if let Some(found) = node
                        .default
                        .as_deref()
                        .and_then(|graph| find_options(root, graph, consumer_id, &nested))
                    {
                        return Some(found);
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
        None
    }

    let mut options = find_options(graph, graph, consumer_id, &[]).unwrap_or_default();
    options.sort_by(|left, right| left.0.cmp(&right.0));
    options.dedup_by(|left, right| left.1 == right.1);
    options
}

fn literal_prototype_for_field(field: &FieldSchema, path: &str) -> serde_json::Value {
    if let Some(value) = field.allowed_values.first() {
        return value.clone();
    }
    literal_prototype_for_type(&field.value_type, path)
}

fn literal_prototype_for_type(value_type: &ContextType, path: &str) -> serde_json::Value {
    match value_type {
        ContextType::Any => serde_json::Value::String(String::new()),
        ContextType::Null => serde_json::Value::Null,
        ContextType::Boolean => serde_json::Value::Bool(false),
        ContextType::Integer => serde_json::json!(1),
        ContextType::Number => serde_json::json!(1.0),
        ContextType::String { format } => serde_json::Value::String(match format {
            Some(SemanticFormat::Path) => "$HOME/example".into(),
            Some(SemanticFormat::FilePath) => "$HOME/example.txt".into(),
            Some(SemanticFormat::DirectoryPath) => "$HOME/Developer".into(),
            Some(SemanticFormat::Url) => "https://example.com".into(),
            Some(SemanticFormat::GitUrl) => "https://github.com/owner/repository.git".into(),
            Some(SemanticFormat::SecretRef) => "profile".into(),
            Some(SemanticFormat::Sha256) => "0".repeat(64),
            Some(SemanticFormat::DateTime) => "2026-01-01T00:00:00Z".into(),
            Some(SemanticFormat::Duration) => "1s".into(),
            Some(SemanticFormat::Email) => "user@example.com".into(),
            Some(SemanticFormat::Hostname) => "example.com".into(),
            Some(SemanticFormat::IpAddress) => "127.0.0.1".into(),
            Some(SemanticFormat::Uuid) => "00000000-0000-0000-0000-000000000000".into(),
            Some(SemanticFormat::GitRef) => "main".into(),
            Some(SemanticFormat::RepositoryName) => "owner/repository".into(),
            Some(SemanticFormat::Identifier) => "value".into(),
            None if path.ends_with("version") => "1.0".into(),
            None if path.ends_with("app_name") => "Application.app".into(),
            None => "value".into(),
        }),
        ContextType::Array { .. } => serde_json::json!([]),
        ContextType::Object { schema } => {
            let mut object = serde_json::Map::new();
            let has_required = schema.fields.values().any(|field| field.required);
            for (name, field) in &schema.fields {
                if field.required || (!has_required && object.is_empty()) {
                    let child_path = join_context_path(path, name);
                    object.insert(
                        name.clone(),
                        literal_prototype_for_field(field, &child_path),
                    );
                }
            }
            serde_json::Value::Object(object)
        }
    }
}

fn step_input_default_value(step: &Step, target: &str) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(step).ok()?;
    for segment in target.split('.') {
        value = value.as_object()?.get(segment)?.clone();
    }
    Some(value)
}

fn manual_input_initial_value(step: &Step, target: &str, field: &FieldSchema) -> serde_json::Value {
    step_input_default_value(step, target)
        .filter(|value| !value.is_null() && validate_literal_binding(value, field).is_ok())
        .unwrap_or_else(|| literal_prototype_for_field(field, target))
}

fn literal_value_label(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn apply_literal_policy_implications(step: &mut Step, target: &str, value: &serde_json::Value) {
    if target == "shell"
        && value.as_str() == Some("allow")
        && matches!(step.action, Action::RunCommand { .. })
    {
        // Shell mode is represented by a typed input binding while the safety
        // marker is step metadata. Keep the authored node valid immediately
        // after the user selects the constrained `allow` value.
        step.dangerous = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphSwitchScalarKind {
    Null,
    Boolean,
    Integer,
    Number,
    String,
}

impl GraphSwitchScalarKind {
    const ALL: [Self; 5] = [
        Self::Null,
        Self::Boolean,
        Self::Integer,
        Self::Number,
        Self::String,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "bool",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
        }
    }

    fn from_context_type(value_type: &ContextType) -> Option<Self> {
        match value_type {
            ContextType::Null => Some(Self::Null),
            ContextType::Boolean => Some(Self::Boolean),
            ContextType::Integer => Some(Self::Integer),
            ContextType::Number => Some(Self::Number),
            ContextType::String { .. } => Some(Self::String),
            ContextType::Any | ContextType::Array { .. } | ContextType::Object { .. } => None,
        }
    }

    fn from_value(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Null => Some(Self::Null),
            serde_json::Value::Bool(_) => Some(Self::Boolean),
            serde_json::Value::Number(value) if value.is_i64() || value.is_u64() => {
                Some(Self::Integer)
            }
            serde_json::Value::Number(_) => Some(Self::Number),
            serde_json::Value::String(_) => Some(Self::String),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
        }
    }

    fn default_value(self, ordinal: usize) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Boolean => serde_json::Value::Bool(ordinal.is_multiple_of(2)),
            Self::Integer => serde_json::json!(ordinal as i64),
            Self::Number => serde_json::json!(ordinal as f64),
            Self::String => serde_json::json!(format!("value-{ordinal}")),
        }
    }

    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            // Integer case literals are valid for a numeric selector too.
            Self::Number => matches!(Self::from_value(value), Some(Self::Integer | Self::Number)),
            _ => Self::from_value(value) == Some(self),
        }
    }
}

#[cfg(test)]
fn graph_switch_selector_kind(
    selector: &Binding,
    options: &[ComposerGraphBindingOption],
) -> Option<GraphSwitchScalarKind> {
    graph_switch_selector_contract(selector, options).map(|(kind, _)| kind)
}

fn graph_switch_selector_contract(
    selector: &Binding,
    options: &[ComposerGraphBindingOption],
) -> Option<(GraphSwitchScalarKind, bool)> {
    match selector {
        Binding::Literal { value } => {
            GraphSwitchScalarKind::from_value(value).map(|kind| (kind, false))
        }
        Binding::Field { .. } => options
            .iter()
            .find(|option| option.binding == *selector)
            .and_then(|option| {
                GraphSwitchScalarKind::from_context_type(&option.value_type)
                    .map(|kind| (kind, option.nullable))
            }),
        Binding::Template { .. } | Binding::Interpolated { .. } => {
            Some((GraphSwitchScalarKind::String, false))
        }
    }
}

fn graph_switch_case_value_compatible(
    selector_kind: GraphSwitchScalarKind,
    selector_nullable: bool,
    value: &serde_json::Value,
) -> bool {
    (selector_nullable && value.is_null()) || selector_kind.accepts(value)
}

fn first_free_switch_case_id(cases: &[SwitchCase]) -> String {
    let used = cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    (1usize..)
        .map(|ordinal| format!("case-{ordinal}"))
        .find(|candidate| !used.contains(candidate.as_str()))
        .expect("unbounded case id sequence has a free value")
}

fn first_free_switch_value_from_used(
    kind: GraphSwitchScalarKind,
    used: &[serde_json::Value],
) -> Option<serde_json::Value> {
    let available = |candidate: &serde_json::Value| !used.iter().any(|value| value == candidate);
    match kind {
        GraphSwitchScalarKind::Null => {
            available(&serde_json::Value::Null).then_some(serde_json::Value::Null)
        }
        GraphSwitchScalarKind::Boolean => [false, true]
            .into_iter()
            .map(serde_json::Value::Bool)
            .find(available),
        GraphSwitchScalarKind::Integer => (1usize..)
            .map(|ordinal| serde_json::json!(ordinal as i64))
            .find(available),
        GraphSwitchScalarKind::Number => (1usize..)
            .map(|ordinal| serde_json::json!(ordinal as f64 + 0.5))
            .find(available),
        GraphSwitchScalarKind::String => (1usize..)
            .map(|ordinal| serde_json::json!(format!("value-{ordinal}")))
            .find(available),
    }
}

fn first_free_switch_case_value(
    kind: GraphSwitchScalarKind,
    cases: &[SwitchCase],
) -> Option<serde_json::Value> {
    let used = cases
        .iter()
        .flat_map(|case| case.values.iter().cloned())
        .collect::<Vec<_>>();
    first_free_switch_value_from_used(kind, &used)
}

fn graph_input_fields(schema: &ObjectSchema) -> Vec<(String, FieldSchema)> {
    fn visit(
        schema: &ObjectSchema,
        prefix: &str,
        inherited_required: bool,
        inherited_sensitivity: Sensitivity,
        output: &mut Vec<(String, FieldSchema)>,
    ) {
        for (name, field) in &schema.fields {
            let target = join_context_path(prefix, name);
            let required = inherited_required && field.required;
            let sensitivity = inherited_sensitivity.combine(field.sensitivity);
            match &field.value_type {
                ContextType::Object { schema }
                    if !schema.fields.is_empty()
                        && ((field.required && !field.nullable)
                            || schema.fields.values().all(|child| !child.required)) =>
                {
                    visit(schema, &target, required, sensitivity, output)
                }
                _ => output.push((
                    target,
                    FieldSchema {
                        value_type: field.value_type.clone(),
                        required,
                        // Input bindings materialize optional/nullable parent
                        // objects. Only the leaf controls whether writing null
                        // is valid; this mirrors resolve_input_target_owned.
                        nullable: field.nullable,
                        description: field.description.clone(),
                        sensitivity,
                        allowed_values: field.allowed_values.clone(),
                    },
                )),
            }
        }
    }

    let mut fields = Vec::new();
    visit(schema, "", true, Sensitivity::Public, &mut fields);
    fields
}

fn paint_graph_literal_editor(
    ui: &mut egui::Ui,
    widget_id: (&str, &str),
    value: &mut serde_json::Value,
) -> bool {
    match value {
        serde_json::Value::String(value) => ui
            .add(egui::TextEdit::singleline(value).desired_width(ui.available_width()))
            .changed(),
        serde_json::Value::Bool(value) => ui.checkbox(value, "").changed(),
        serde_json::Value::Number(value) if value.is_i64() || value.is_u64() => {
            let mut integer = value.as_i64().unwrap_or_default();
            let changed = ui.add(egui::DragValue::new(&mut integer)).changed();
            if changed {
                *value = serde_json::Number::from(integer);
            }
            changed
        }
        serde_json::Value::Number(value) => {
            let mut number = value.as_f64().unwrap_or_default();
            let changed = ui.add(egui::DragValue::new(&mut number)).changed();
            if changed {
                if let Some(parsed) = serde_json::Number::from_f64(number) {
                    *value = parsed;
                }
            }
            changed
        }
        serde_json::Value::Null => {
            ui.label(RichText::new("null").monospace().size(9.0).color(MUTED));
            false
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            let key = Id::new(("graph-json-literal", widget_id.0, widget_id.1));
            let error_key = Id::new(("graph-json-literal-error", widget_id.0, widget_id.1));
            let initial = serde_json::to_string_pretty(value).unwrap_or_default();
            let mut source = ui
                .data_mut(|data| data.get_temp::<String>(key))
                .unwrap_or(initial);
            let response = ui.add(
                egui::TextEdit::multiline(&mut source)
                    .font(egui::TextStyle::Monospace)
                    .char_limit(16 * 1024)
                    .desired_rows(4)
                    .desired_width(ui.available_width()),
            );
            ui.data_mut(|data| data.insert_temp(key, source.clone()));
            let mut changed = false;
            if response.changed() {
                match serde_json::from_str(&source) {
                    Ok(parsed) => {
                        *value = parsed;
                        changed = true;
                        ui.data_mut(|data| data.remove::<String>(error_key));
                    }
                    Err(error) => {
                        ui.data_mut(|data| {
                            data.insert_temp(error_key, format!("JSON: {error}"));
                        });
                    }
                }
            }
            if let Some(error) = ui.data_mut(|data| data.get_temp::<String>(error_key)) {
                ui.add(egui::Label::new(RichText::new(error).size(8.0).color(ORANGE)).truncate());
            }
            changed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchCaseHeaderAction {
    MoveUp,
    MoveDown,
    Remove,
    OpenBranch,
}

fn paint_switch_case_header(
    ui: &mut egui::Ui,
    case_id: &str,
    index: usize,
    case_count: usize,
    branch_empty: bool,
) -> Option<SwitchCaseHeaderAction> {
    let mut action = None;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.button("＋ В ветку").clicked() {
            action = Some(SwitchCaseHeaderAction::OpenBranch);
        }
        if ui
            .add_enabled(case_count > 1 && branch_empty, egui::Button::new("Удалить"))
            .on_disabled_hover_text(if case_count == 1 {
                "SWITCH должен содержать хотя бы один case."
            } else {
                "Сначала удалите или перенесите блоки из ветки case."
            })
            .clicked()
        {
            action = Some(SwitchCaseHeaderAction::Remove);
        }
        if ui
            .add_enabled(index + 1 < case_count, egui::Button::new("↓"))
            .clicked()
        {
            action = Some(SwitchCaseHeaderAction::MoveDown);
        }
        if ui.add_enabled(index > 0, egui::Button::new("↑")).clicked() {
            action = Some(SwitchCaseHeaderAction::MoveUp);
        }
        ui.add(
            egui::Label::new(RichText::new(case_id).monospace().size(9.0).color(PURPLE)).truncate(),
        );
    });
    action
}

fn paint_switch_case_value_row(
    ui: &mut egui::Ui,
    widget_id: (&str, &str),
    value_index: usize,
    value_count: usize,
    value: &mut serde_json::Value,
    selector_contract: Option<(GraphSwitchScalarKind, bool)>,
    used_case_values: &mut Vec<serde_json::Value>,
) -> (bool, bool) {
    let (node_id, case_id) = widget_id;
    let selector_kind = selector_contract.map(|(kind, _)| kind);
    let selector_nullable = selector_contract.is_some_and(|(_, nullable)| nullable);
    let compatible = selector_kind
        .is_none_or(|kind| graph_switch_case_value_compatible(kind, selector_nullable, value));
    let mut remove = false;
    let mut replace = false;
    let mut changed = false;
    let widget_key = format!("{case_id}-{value_index}");
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
            .add_enabled(value_count > 1, egui::Button::new("−"))
            .clicked()
        {
            remove = true;
        }
        if !compatible && ui.button("Заменить").clicked() {
            replace = true;
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Значение {}", value_index + 1))
                        .size(8.0)
                        .color(MUTED),
                )
                .truncate(),
            );
            if compatible {
                changed |= paint_graph_literal_editor(ui, (node_id, &widget_key), value);
            } else if let Some(kind) = selector_kind {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("несовместимо с {}", kind.label()))
                            .size(8.0)
                            .color(ORANGE),
                    )
                    .truncate(),
                );
            }
        });
    });
    if replace {
        if let Some(kind) = selector_kind {
            if let Some(replacement) = first_free_switch_value_from_used(kind, used_case_values) {
                *value = replacement.clone();
                used_case_values.push(replacement);
                changed = true;
            }
        }
    }
    (changed, remove)
}

fn paint_join_source_row(
    ui: &mut egui::Ui,
    join_id: &str,
    source: &GraphNode,
    existing: Option<EdgePort>,
) -> Option<Option<EdgePort>> {
    let mut change = None;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if let Some(mut port) = existing.clone() {
            egui::ComboBox::from_id_salt(("join-source-port", join_id, source.id()))
                .selected_text(graph_edge_port_label(&port))
                .truncate()
                .width(ui.available_width().min(112.0))
                .show_ui(ui, |ui| {
                    for candidate in graph_node_output_ports(source) {
                        if ui
                            .selectable_value(
                                &mut port,
                                candidate.clone(),
                                graph_edge_port_label(&candidate),
                            )
                            .changed()
                        {
                            change = Some(Some(candidate));
                        }
                    }
                });
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let mut connected = existing.is_some();
            let checkbox_changed = ui.checkbox(&mut connected, "").changed();
            let label_clicked = ui
                .add(
                    egui::Label::new(RichText::new(source.id()).monospace().size(8.0))
                        .truncate()
                        .sense(Sense::click()),
                )
                .clicked();
            if label_clicked {
                connected = !connected;
            }
            if checkbox_changed || label_clicked {
                change = Some(connected.then(|| graph_node_flow_port(source)));
            }
        });
    });
    change
}

fn auth_policy_label(policy: AuthPolicy) -> &'static str {
    match policy {
        AuthPolicy::None => "Нет",
        AuthPolicy::GitCredential => "Git credentials",
        AuthPolicy::Sudo => "Sudo",
    }
}

fn graph_step_policy_issues(step: &Step, capabilities: &BlockPolicyCapabilities) -> Vec<String> {
    let mut issues = Vec::new();
    if !capabilities.allows_auth(step.auth) {
        issues.push(format!(
            "Аутентификация «{}» недоступна этому блоку.",
            auth_policy_label(step.auth)
        ));
    }
    if !capabilities.allow_elevation && !matches!(step.allow_elevation, ElevationPolicy::Forbidden)
    {
        issues.push("Повышение прав должно быть запрещено.".into());
    }
    if !capabilities.allows_dangerous(step.dangerous) {
        issues.push(match capabilities.dangerous {
            PolicyRequirement::Forbidden => "Блок нельзя помечать опасной операцией.".into(),
            PolicyRequirement::Required => {
                "Для этого блока обязательна отметка «Опасная операция».".into()
            }
            PolicyRequirement::Optional => unreachable!("optional dangerous policy accepts both"),
        });
    }
    issues
}

fn reset_graph_step_policy(step: &mut Step, capabilities: &BlockPolicyCapabilities) {
    if !capabilities.allows_auth(step.auth) {
        step.auth = AuthPolicy::None;
    }
    if !capabilities.allow_elevation {
        step.allow_elevation = ElevationPolicy::Forbidden;
    }
    step.dangerous = match capabilities.dangerous {
        PolicyRequirement::Forbidden => false,
        PolicyRequirement::Optional => step.dangerous,
        PolicyRequirement::Required => true,
    };
}

fn paint_graph_action_editor(
    ui: &mut egui::Ui,
    node: &mut ActionNode,
    options: &BTreeMap<String, Vec<ComposerGraphBindingOption>>,
    dark: bool,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new("Название блока").size(9.0).color(MUTED));
    changed |= ui
        .add(egui::TextEdit::singleline(&mut node.step.name).desired_width(ui.available_width()))
        .changed();
    ui.label(RichText::new("ID блока").size(9.0).color(MUTED));
    ui.add(
        egui::Label::new(
            RichText::new(&node.step.id)
                .monospace()
                .size(9.0)
                .color(PURPLE),
        )
        .truncate(),
    );
    ui.add(
        egui::Label::new(
            RichText::new("ID стабилен: связи и контекст адресуют узел по нему.")
                .size(8.0)
                .color(MUTED),
        )
        .wrap(),
    );
    ui.add_space(8.0);
    section_label(ui, "ВХОДЫ ПО СХЕМЕ");
    let definition = definition_for_action(&node.step.action);
    if definition.input_schema.fields.is_empty() {
        ui.label(
            RichText::new("У блока нет входных параметров.")
                .size(9.0)
                .color(MUTED),
        );
    }
    for (target, field) in graph_input_fields(&definition.input_schema) {
        ui.add_space(5.0);
        ui.vertical(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&target).monospace().size(9.0).color(PURPLE))
                    .truncate(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(context_type_label(
                        &field.value_type,
                        field.nullable,
                        !field.required,
                    ))
                    .monospace()
                    .size(8.0)
                    .color(MUTED),
                )
                .truncate(),
            );
        });
        let compatible = options.get(&target).map(Vec::as_slice).unwrap_or_default();
        let current = node.bindings.get(&target).cloned();
        let manual_initial = manual_input_initial_value(&node.step, &target, &field);
        let selected_label = match &current {
            None if field.required => "Значение блока по умолчанию".to_owned(),
            None => "Не задавать · оставить default".to_owned(),
            Some(Binding::Literal {
                value: serde_json::Value::Null,
            }) if field.nullable => "Явно null".to_owned(),
            Some(Binding::Literal { .. }) => "Вручную".to_owned(),
            Some(binding) => compatible
                .iter()
                .find(|option| option.binding == *binding)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| "Привязка из YAML".into()),
        };
        egui::ComboBox::from_id_salt(("graph-input", &node.step.id, &target))
            .selected_text(selected_label)
            .truncate()
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        current.is_none(),
                        if field.required {
                            "Значение блока по умолчанию"
                        } else {
                            "Не задавать · оставить default"
                        },
                    )
                    .clicked()
                {
                    node.bindings.remove(&target);
                    changed = true;
                    ui.close();
                }
                if ui
                    .selectable_label(
                        matches!(current.as_ref(), Some(Binding::Literal { .. })),
                        "Вручную",
                    )
                    .clicked()
                {
                    node.bindings
                        .insert(target.clone(), Binding::literal(manual_initial.clone()));
                    changed = true;
                    ui.close();
                }
                if field.nullable
                    && ui
                        .selectable_label(
                            matches!(
                                current.as_ref(),
                                Some(Binding::Literal {
                                    value: serde_json::Value::Null
                                })
                            ),
                            "Явно null",
                        )
                        .clicked()
                {
                    node.bindings
                        .insert(target.clone(), Binding::literal(serde_json::Value::Null));
                    changed = true;
                    ui.close();
                }
                for option in compatible {
                    if ui
                        .selectable_label(current.as_ref() == Some(&option.binding), &option.label)
                        .clicked()
                    {
                        node.bindings.insert(target.clone(), option.binding.clone());
                        changed = true;
                        ui.close();
                    }
                }
            });
        let mut enum_literal_changed = false;
        let mut selected_literal = None;
        if let Some(Binding::Literal { value }) = node.bindings.get_mut(&target) {
            if field.allowed_values.is_empty() {
                changed |= paint_graph_literal_editor(ui, (&node.step.id, &target), value);
            } else {
                egui::ComboBox::from_id_salt(("graph-enum-literal", &node.step.id, &target))
                    .selected_text(literal_value_label(value))
                    .truncate()
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for allowed in &field.allowed_values {
                            let option_changed = ui
                                .selectable_value(
                                    value,
                                    allowed.clone(),
                                    literal_value_label(allowed),
                                )
                                .changed();
                            enum_literal_changed |= option_changed;
                            changed |= option_changed;
                        }
                    });
                selected_literal = Some(value.clone());
            }
            if let Err(error) = validate_literal_binding(value, &field) {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Некорректное значение: {error}"))
                            .size(8.0)
                            .color(ORANGE),
                    )
                    .truncate(),
                );
            }
        } else if compatible.is_empty() && current.is_some() {
            ui.label(
                RichText::new("Текущая привязка не входит в видимый типизированный контекст.")
                    .size(8.0)
                    .color(ORANGE),
            );
        }
        if enum_literal_changed {
            if let Some(value) = &selected_literal {
                apply_literal_policy_implications(&mut node.step, &target, value);
            }
        }
    }
    ui.add_space(8.0);
    section_label(ui, "ПОЛИТИКИ ШАГА");
    let policy_issues = graph_step_policy_issues(&node.step, &definition.policy);
    if !policy_issues.is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "Сохранённые политики недопустимы:\n• {}",
                    policy_issues.join("\n• ")
                ))
                .size(8.0)
                .color(ORANGE),
            )
            .truncate(),
        );
        if ui
            .button("Сбросить политики к безопасным")
            .on_hover_text("Изменение произойдёт только после этого клика.")
            .clicked()
        {
            reset_graph_step_policy(&mut node.step, &definition.policy);
            changed = true;
        }
        ui.add_space(5.0);
    }
    ui.label(RichText::new("Аутентификация").size(9.0).color(MUTED));
    egui::ComboBox::from_id_salt(("graph-step-auth", &node.step.id))
        .selected_text(auth_policy_label(node.step.auth))
        .truncate()
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(&mut node.step.auth, AuthPolicy::None, "Нет")
                .changed();
            if definition.policy.allow_git_credentials {
                changed |= ui
                    .selectable_value(
                        &mut node.step.auth,
                        AuthPolicy::GitCredential,
                        "Git credentials",
                    )
                    .changed();
            }
            if definition.policy.allow_sudo {
                changed |= ui
                    .selectable_value(&mut node.step.auth, AuthPolicy::Sudo, "Sudo")
                    .changed();
            }
        });
    ui.label(RichText::new("Повышение прав").size(9.0).color(MUTED));
    if definition.policy.allow_elevation {
        egui::ComboBox::from_id_salt(("graph-step-elevation", &node.step.id))
            .selected_text(match node.step.allow_elevation {
                ElevationPolicy::Forbidden => "Запрещено",
                ElevationPolicy::Allow => "Разрешено",
            })
            .truncate()
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(
                        &mut node.step.allow_elevation,
                        ElevationPolicy::Forbidden,
                        "Запрещено",
                    )
                    .changed();
                changed |= ui
                    .selectable_value(
                        &mut node.step.allow_elevation,
                        ElevationPolicy::Allow,
                        "Разрешено",
                    )
                    .changed();
            });
    } else {
        ui.label(
            RichText::new("Запрещено для этого блока")
                .size(8.0)
                .color(MUTED),
        );
    }
    match definition.policy.dangerous {
        PolicyRequirement::Optional => {
            changed |= ui
                .checkbox(&mut node.step.dangerous, "Опасная операция")
                .changed();
        }
        PolicyRequirement::Forbidden => {
            ui.label(
                RichText::new("Опасная операция: запрещено для этого блока")
                    .size(8.0)
                    .color(MUTED),
            );
        }
        PolicyRequirement::Required => {
            ui.label(
                RichText::new("Опасная операция: обязательная защита включена")
                    .size(8.0)
                    .color(MUTED),
            );
        }
    }
    if let Err(error) = node.step.validate() {
        ui.add(
            egui::Label::new(
                RichText::new(if definition.policy.accepts(&node.step) {
                    format!("Проверьте обязательные параметры блока: {error}")
                } else {
                    "Политика блока требует исправления кнопкой выше.".into()
                })
                .size(8.0)
                .color(ORANGE),
            )
            .truncate(),
        );
    }
    ui.add_space(8.0);
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "{} · schema v{}",
                definition.kind.id(),
                definition.schema_version
            ))
            .monospace()
            .size(8.0)
            .color(if changed { PURPLE } else { text(dark) }),
        )
        .truncate(),
    );
    changed
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
    authorization_intent: GithubAuthorizationIntent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum GithubAuthorizationIntent {
    #[default]
    RepositoryPicker,
    RetryScenario,
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
            authorization_intent: GithubAuthorizationIntent::RepositoryPicker,
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
    /// Stable graph-node selection for graph-native custom scenarios. Legacy
    /// library tasks retain `selected_step` because their reports are linear.
    selected_node: Option<String>,
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
    graph_picker_attach: Option<ComposerGraphAttach>,
    graph_picker_port: Option<EdgePort>,
    block_picker_search: String,
    readonly_canvas_views: BTreeMap<String, CanvasView>,
}

impl ScenarioApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_unicode_fonts(&cc.egui_ctx);
        configure_styles(&cc.egui_ctx, egui::ThemePreference::System);
        let dark = cc.egui_ctx.theme() == egui::Theme::Dark;
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
            selected_node: None,
            channel: ReleaseChannel::Release,
            allow_shell: false,
            allow_elevation: false,
            report: None,
            report_applied: false,
            plan_error: None,
            dark,
            confirm_run: false,
            running: false,
            run_receiver: None,
            github_picker: GithubPickerState::default(),
            file_message: None,
            custom_project: None,
            selected_project_scenario: None,
            selected_project_group: Vec::new(),
            block_picker_parent: None,
            graph_picker_attach: None,
            graph_picker_port: None,
            block_picker_search: String::new(),
            readonly_canvas_views: BTreeMap::new(),
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
            graph: Some(WorkflowGraph::default()),
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
                entries: vec![ProjectEntry::Scenario {
                    task: Box::new(task),
                }],
            }],
        });
        self.selected_project_scenario = Some(vec![0, 0]);
        self.selected_project_group = vec![0];
        self.selected_step = None;
        self.selected_node = None;
        self.github_picker.open = false;
        self.github_picker.selected_ids.clear();
        self.invalidate_plan();
    }

    /// Retained only for tests that exercise the schema-v1 import helpers.
    /// Production authoring is exclusively routed through `add_graph_composer_block`.
    #[cfg(test)]
    #[allow(dead_code)]
    fn add_composer_block(&mut self, kind: ComposerBlockKind) {
        if self
            .selected_task()
            .is_some_and(|task| task.graph.is_some())
        {
            let attach = self
                .graph_picker_attach
                .clone()
                .unwrap_or(ComposerGraphAttach::RootStart);
            let source_port = self.graph_picker_port.clone();
            let selected_path = self.selected_project_scenario.clone();
            let Some(task) = self
                .custom_project
                .as_mut()
                .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
            else {
                return;
            };
            let task_id = task.id.clone();
            let Some(graph) = task.graph.as_mut() else {
                return;
            };
            let graph_kind = if matches!(kind, ComposerBlockKind::ForEach) {
                ComposerGraphBlockKind::ForEach
            } else {
                ComposerGraphBlockKind::Action(kind.action_kind())
            };
            match graph_insert_composer_block(graph, &attach, source_port, graph_kind) {
                Ok(id) => {
                    self.selected_node = Some(id.clone());
                    self.selected_step = None;
                    if let Some(project) = self.custom_project.as_mut() {
                        let canvas = project.canvases.entry(task_id).or_default();
                        let parent_id = match &attach {
                            ComposerGraphAttach::RootStart => "start",
                            ComposerGraphAttach::RootAfter { node_id }
                            | ComposerGraphAttach::NestedAfter { node_id, .. } => node_id,
                            ComposerGraphAttach::NestedStart { scope } => scope.owner_id(),
                        };
                        let parent = canvas
                            .positions
                            .get(parent_id)
                            .copied()
                            .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
                        let iteration_offset = matches!(
                            attach,
                            ComposerGraphAttach::NestedStart { .. }
                                | ComposerGraphAttach::NestedAfter { .. }
                        );
                        canvas.positions.insert(
                            id,
                            CanvasPoint {
                                x: parent.x + 286.0,
                                y: parent.y + if iteration_offset { 158.0 } else { 0.0 },
                            },
                        );
                    }
                }
                Err(error) => self.file_message = Some((true, error)),
            }
            self.graph_picker_attach = None;
            self.graph_picker_port = None;
            self.block_picker_parent = None;
            self.block_picker_search.clear();
            self.invalidate_plan();
            return;
        }

        let parent = self
            .block_picker_parent
            .clone()
            .unwrap_or_else(|| "start".into());
        let selected_path = self.selected_project_scenario.clone();
        let composer_canvas = self.custom_project.as_ref().and_then(|project| {
            let task = project.scenario(selected_path.as_deref()?)?;
            project.canvases.get(&task.id).cloned()
        });
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
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
        let mut new_step = composer_step(kind, id.clone());
        if matches!(kind, ComposerBlockKind::ForEach) {
            let source =
                composer_array_sources_scoped(task, task.steps.len(), composer_canvas.as_ref())
                    .into_iter()
                    .find(|source| source.step_id == parent);
            if let (
                Some(source),
                Action::ForEach {
                    source_step,
                    array_path,
                    item,
                    fields,
                },
            ) = (source, &mut new_step.action)
            {
                *source_step = source.step_id;
                *array_path = source.path;
                *item = source.item;
                *fields = item_object_fields(&source.item_type)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
            }
        } else if matches!(kind, ComposerBlockKind::GitInspect) {
            composer_bind_git_inspect_to_parent_loop(task, &parent, &mut new_step);
        } else if matches!(kind, ComposerBlockKind::GitCloneIfMissing)
            && task
                .steps
                .iter()
                .any(|step| step.id == parent && matches!(step.action, Action::ForEach { .. }))
        {
            new_step.action = Action::ForEachGitCloneIfMissing {
                loop_step: parent.clone(),
                repo: "{{repository.https_url}}".into(),
                dest: "$HOME/Developer/{{repository.owner}}/{{repository.name}}".into(),
                branch: Some("{{repository.default_branch}}".into()),
            };
        }
        task.steps.push(new_step);
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
        self.graph_picker_attach = None;
        self.graph_picker_port = None;
        self.block_picker_search.clear();
        self.invalidate_plan();
    }

    fn add_graph_composer_block(&mut self, kind: ComposerGraphBlockKind) {
        let attach = self
            .graph_picker_attach
            .clone()
            .unwrap_or(ComposerGraphAttach::RootStart);
        let source_port = self.graph_picker_port.clone();
        let selected_path = self.selected_project_scenario.clone();
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
        else {
            return;
        };
        let task_id = task.id.clone();
        let Some(graph) = task.graph.as_mut() else {
            return;
        };
        let mut continue_with_loop_body = None;
        match graph_insert_composer_block(graph, &attach, source_port, kind) {
            Ok(id) => {
                if matches!(kind, ComposerGraphBlockKind::ForEach) {
                    continue_with_loop_body = Some(ComposerGraphAttach::NestedStart {
                        scope: ComposerGraphNestedScope::ForEachBody {
                            owner_id: id.clone(),
                        },
                    });
                }
                self.selected_node = Some(id.clone());
                self.selected_step = None;
                if let Some(project) = self.custom_project.as_mut() {
                    let canvas = project.canvases.entry(task_id).or_default();
                    let parent_id = match &attach {
                        ComposerGraphAttach::RootStart => "start",
                        ComposerGraphAttach::RootAfter { node_id }
                        | ComposerGraphAttach::NestedAfter { node_id, .. } => node_id,
                        ComposerGraphAttach::NestedStart { scope } => scope.owner_id(),
                    };
                    let parent = canvas
                        .positions
                        .get(parent_id)
                        .copied()
                        .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
                    let iteration_offset = matches!(
                        attach,
                        ComposerGraphAttach::NestedStart { .. }
                            | ComposerGraphAttach::NestedAfter { .. }
                    );
                    canvas.positions.insert(
                        id,
                        CanvasPoint {
                            x: parent.x + 286.0,
                            y: parent.y + if iteration_offset { 158.0 } else { 0.0 },
                        },
                    );
                }
            }
            Err(error) => self.file_message = Some((true, error)),
        }
        self.graph_picker_attach = None;
        self.graph_picker_port = None;
        self.block_picker_parent = None;
        self.block_picker_search.clear();
        self.invalidate_plan();
        if let Some(attach) = continue_with_loop_body {
            self.open_graph_block_picker(attach);
        }
    }

    fn open_graph_block_picker(&mut self, attach: ComposerGraphAttach) {
        if self.running || self.custom_project.is_none() {
            return;
        }
        if let Some(message) = self
            .selected_task()
            .and_then(|task| task.graph.as_ref())
            .and_then(|graph| graph_attach_blocker(graph, &attach))
        {
            self.file_message = Some((true, message));
            return;
        }
        let label = match &attach {
            ComposerGraphAttach::RootStart => "start".to_owned(),
            ComposerGraphAttach::RootAfter { node_id } => node_id.clone(),
            ComposerGraphAttach::NestedStart { scope } => match scope {
                ComposerGraphNestedScope::ForEachBody { .. } => {
                    format!("{} · для каждого item", scope.owner_id())
                }
                _ => format!("{} · ветка", scope.owner_id()),
            },
            ComposerGraphAttach::NestedAfter { node_id, .. } => node_id.clone(),
        };
        self.graph_picker_port = self
            .selected_task()
            .and_then(|task| task.graph.as_ref())
            .and_then(|graph| match &attach {
                ComposerGraphAttach::RootAfter { node_id }
                | ComposerGraphAttach::NestedAfter { node_id, .. } => {
                    graph_node(graph, node_id).map(graph_node_flow_port)
                }
                ComposerGraphAttach::RootStart | ComposerGraphAttach::NestedStart { .. } => None,
            });
        self.block_picker_parent = Some(label);
        self.graph_picker_attach = Some(attach);
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

        let Some(graph) = &task.graph else {
            // Task.steps is accepted only by the YAML importer. It is never a
            // second authoring model for the canvas.
            canvas.parents.clear();
            return;
        };
        let (nodes, edges) = graph_visual_model(graph);
        let valid_ids = nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        canvas
            .positions
            .retain(|id, _| id == "start" || valid_ids.contains(id.as_str()));
        canvas.parents.clear();
        for (index, node) in nodes.iter().enumerate() {
            if canvas.positions.contains_key(&node.id) {
                continue;
            }
            let parent_id = edges
                .iter()
                .find(|edge| edge.to == node.id)
                .map(|edge| edge.from.as_str())
                .unwrap_or("start");
            let parent = canvas
                .positions
                .get(parent_id)
                .copied()
                .unwrap_or(CanvasPoint { x: 80.0, y: 250.0 });
            let iteration = edges
                .iter()
                .any(|edge| edge.to == node.id && edge.kind == ComposerGraphEdgeKind::Iteration);
            canvas.positions.insert(
                node.id.clone(),
                CanvasPoint {
                    x: parent.x + 286.0,
                    y: parent.y
                        + if iteration {
                            158.0
                        } else {
                            branch_offset(index)
                        },
                },
            );
        }
    }

    fn set_composer_canvas_view(&mut self, task_id: &str, view: CanvasView) {
        let Some(canvas) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.canvases.get_mut(task_id))
        else {
            return;
        };
        canvas.view = view.sanitized();
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

    fn remove_composer_node(&mut self, node_id: &str) {
        let selected_path = self.selected_project_scenario.clone();
        let Some(task) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
        else {
            return;
        };
        let Some(graph) = task.graph.as_mut() else {
            return;
        };
        let removed_ids = graph_node(graph, node_id)
            .map(|node| {
                let mut nested = WorkflowGraph::default();
                nested.nodes.push(node.clone());
                graph_node_ids(&nested)
            })
            .unwrap_or_default();
        match graph_remove_composer_node(graph, node_id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.file_message = Some((true, error));
                return;
            }
        }
        let task_id = task.id.clone();
        if let Some(canvas) = self
            .custom_project
            .as_mut()
            .and_then(|project| project.canvases.get_mut(&task_id))
        {
            for id in removed_ids {
                canvas.positions.remove(&id);
            }
        }
        self.selected_node = None;
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
            task: Box::new(Task {
                id: format!("scenario-{ordinal}"),
                name: format!("Новый сценарий {ordinal}"),
                description: "Сценарий, собранный из атомарных операций в ppduster.".into(),
                platform: ppduster::rules::Platform::Macos,
                trust: TrustRequirement::ExternalAllowed,
                scenarios: Vec::new(),
                resolved_scenarios: Vec::new(),
                graph: Some(WorkflowGraph::default()),
                steps: Vec::new(),
            }),
        });
        let mut scenario_path = path;
        scenario_path.push(entries.len() - 1);
        self.selected_project_scenario = Some(scenario_path);
        self.selected_step = None;
        self.selected_node = None;
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
            task: Box::new(github_repository_composer_task(ordinal)),
        });
        let mut scenario_path = path;
        scenario_path.push(entries.len() - 1);
        self.selected_project_scenario = Some(scenario_path);
        self.selected_step = None;
        self.selected_node = Some("list-repositories".into());
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

    fn start_github_authorization(
        &mut self,
        ctx: &egui::Context,
        intent: GithubAuthorizationIntent,
    ) {
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
        self.github_picker.authorization_intent = intent;
        self.github_picker.error = None;
    }

    fn poll_github_authorization(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.github_picker.auth_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(())) => {
                let intent = self.github_picker.authorization_intent;
                self.github_picker.authorizing = false;
                self.github_picker.auth_receiver = None;
                match intent {
                    GithubAuthorizationIntent::RepositoryPicker => {
                        self.start_github_repository_load(ctx);
                    }
                    GithubAuthorizationIntent::RetryScenario => self.start_run(ctx),
                }
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
        if let Some(project) = &self.custom_project {
            if let Err(error) = validate_project(project) {
                self.report = None;
                self.plan_error = Some(error);
                return;
            }
        }
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
            release_channel: task_contains_action(task, &|action| {
                matches!(action, Action::BambuStudioRelease(_))
            })
            .then_some(self.channel),
        }
    }

    fn start_run(&mut self, ctx: &egui::Context) {
        self.report = None;
        self.report_applied = false;
        if let Some(project) = &self.custom_project {
            if let Err(error) = validate_project(project) {
                self.plan_error = Some(error);
                self.confirm_run = false;
                return;
            }
        }
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
        if task_contains_action(&resolved, &|action| {
            matches!(action, Action::BambuStudioRelease(_))
        }) {
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
                // Canvas scope errors remain editable after load; full
                // validation still blocks plan, run, and save until fixed.
                validate_project_for_editing(&project).map_err(anyhow::Error::msg)?;
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
        let selected_task = selected.as_ref().and_then(|path| {
            self.custom_project
                .as_ref()
                .and_then(|project| project.scenario(path))
        });
        self.selected_node = selected_task
            .and_then(|task| task.graph.as_ref())
            .and_then(|graph| graph.nodes.first())
            .map(|node| node.id().to_owned());
        self.selected_step = selected_task
            .is_some_and(|task| task.graph.is_none() && !task.steps.is_empty())
            .then_some(0);
        self.load_error = None;
        self.file_message = Some((false, format!("Проект загружен: {}", path.display())));
    }

    fn block_picker(&mut self, ctx: &egui::Context) {
        let Some(parent) = self.block_picker_parent.clone() else {
            return;
        };
        let source_ports = self
            .graph_picker_attach
            .as_ref()
            .and_then(|attach| match attach {
                ComposerGraphAttach::RootAfter { node_id }
                | ComposerGraphAttach::NestedAfter { node_id, .. } => self
                    .selected_task()
                    .and_then(|task| task.graph.as_ref())
                    .and_then(|graph| graph_node(graph, node_id))
                    .map(graph_node_output_ports),
                ComposerGraphAttach::RootStart | ComposerGraphAttach::NestedStart { .. } => None,
            })
            .unwrap_or_default();
        let picker_height = (ctx.content_rect().height() - 96.0).clamp(540.0, 760.0);
        let list_height = picker_height - 118.0;
        let mut selected_graph = None;
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
                if !source_ports.is_empty() {
                    ui.label(
                        RichText::new("Выход исходного блока")
                            .size(9.0)
                            .color(MUTED),
                    );
                    let mut selected = self
                        .graph_picker_port
                        .clone()
                        .filter(|port| source_ports.contains(port))
                        .unwrap_or_else(|| source_ports[0].clone());
                    egui::ComboBox::from_id_salt("composer-block-source-port")
                        .selected_text(graph_edge_port_label(&selected))
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for port in &source_ports {
                                ui.selectable_value(
                                    &mut selected,
                                    port.clone(),
                                    graph_edge_port_label(port),
                                );
                            }
                        });
                    self.graph_picker_port = Some(selected);
                    ui.add_space(8.0);
                }
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
                        let graph_controls = [
                            (ComposerGraphBlockKind::ForEach, "Для каждого элемента"),
                            (ComposerGraphBlockKind::If, "Если / иначе"),
                            (ComposerGraphBlockKind::Switch, "Выбор по значению"),
                            (ComposerGraphBlockKind::Join, "Объединить ветви"),
                        ]
                        .into_iter()
                        .map(|(kind, title)| {
                            let mut definition = block_definition(ActionKind::ForEach);
                            definition.title = title.into();
                            definition.category = "Логика".into();
                            (kind, definition)
                        });
                        let graph_definitions = graph_controls
                            .chain(
                                ActionKind::ALL
                                    .into_iter()
                                    .filter(|kind| kind.is_graph_action())
                                    .map(|kind| {
                                        (
                                            ComposerGraphBlockKind::Action(kind),
                                            block_definition(kind),
                                        )
                                    }),
                            )
                            .collect::<Vec<_>>();
                        for (graph_kind, definition) in graph_definitions {
                            let context_lines = schema_context_lines(&definition.output_schema);
                            let context_search = context_lines.join(" ");
                            if !query.is_empty()
                                && !definition.title.to_lowercase().contains(&query)
                                && !definition.category.to_lowercase().contains(&query)
                                && !context_search.to_lowercase().contains(&query)
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
                                            RichText::new(&definition.title)
                                                .strong()
                                                .size(11.0)
                                                .color(text(self.dark)),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(&definition.category)
                                                        .size(8.0)
                                                        .color(CYAN),
                                                );
                                            },
                                        );
                                    });
                                    for (index, line) in context_lines.iter().take(4).enumerate() {
                                        let prefix = if index == 0 {
                                            "Выход: "
                                        } else {
                                            "       "
                                        };
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!("{prefix}{line}"))
                                                    .monospace()
                                                    .size(8.0)
                                                    .color(PURPLE),
                                            )
                                            .truncate(),
                                        );
                                    }
                                    if context_lines.len() > 4 {
                                        ui.label(
                                            RichText::new(format!(
                                                "       … ещё {}",
                                                context_lines.len() - 4
                                            ))
                                            .monospace()
                                            .size(8.0)
                                            .color(MUTED),
                                        );
                                    }
                                })
                                .response
                                .interact(Sense::click());
                            if response.clicked() {
                                selected_graph = Some(graph_kind);
                            }
                            ui.add_space(6.0);
                        }
                    });
            });
        if close {
            self.block_picker_parent = None;
            self.graph_picker_attach = None;
            self.graph_picker_port = None;
        } else if let Some(kind) = selected_graph {
            self.add_graph_composer_block(kind);
        }
    }
}

impl eframe::App for ScenarioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep custom colors in sync when the OS appearance changes while the
        // application is using the system theme preference.
        self.dark = ui.ctx().theme() == egui::Theme::Dark;
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
    validate_project_structure(project)?;
    validate_project_canvases(&project.entries, &project.canvases)
}

fn validate_project_canvases(
    entries: &[ProjectEntry],
    canvases: &BTreeMap<String, ComposerCanvas>,
) -> Result<(), String> {
    for entry in entries {
        match entry {
            ProjectEntry::Group { entries, .. } => {
                validate_project_canvases(entries, canvases)?;
            }
            ProjectEntry::Scenario { task } => {
                if let Some(canvas) = canvases.get(&task.id) {
                    validate_composer_canvas(task, canvas)?;
                } else if let Some(graph) = &task.graph {
                    validate_graph_for_ui(task, graph)?;
                }
            }
        }
    }
    Ok(())
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
                            .map(|resolved| task_action_steps(&resolved).len())
                            .unwrap_or_else(|| task_action_steps(task).len());
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
                            configure_styles(
                                ui.ctx(),
                                if self.dark {
                                    egui::ThemePreference::Dark
                                } else {
                                    egui::ThemePreference::Light
                                },
                            );
                        }
                        ui.label(RichText::new("SAFE MODE").strong().size(9.0).color(CYAN));
                    });
                });
            });
    }

    fn left_library(&mut self, root: &mut egui::Ui) {
        let (library_width, _) = workspace_panel_widths(root.max_rect().width());
        egui::Panel::left("library")
            .exact_size(library_width)
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
                    self.selected_node = None;
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
                    let selected_task = self
                        .selected_project_scenario
                        .as_ref()
                        .and_then(|path| project.scenario(path));
                    self.selected_node = selected_task
                        .and_then(|task| task.graph.as_ref())
                        .and_then(|graph| graph.nodes.first())
                        .map(|node| node.id().to_owned());
                    self.selected_step = selected_task
                        .is_some_and(|task| task.graph.is_none() && !task.steps.is_empty())
                        .then_some(0);
                    self.invalidate_plan();
                }
            }
        }
    }

    fn right_inspector(&mut self, root: &mut egui::Ui) {
        let (_, inspector_width) = workspace_panel_widths(root.max_rect().width());
        egui::Panel::right("inspector")
            .exact_size(inspector_width)
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
                    bounded_inspector_scroll(ui, "composer-inspector-scroll", |ui| {
                        self.composer_inspector(ui);
                    });
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
                bounded_inspector_scroll(ui, "library-inspector-scroll", |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&task.name)
                                .strong()
                                .size(18.0)
                                .color(text(self.dark)),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&task.name);
                    ui.add_space(5.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&task.id)
                                .monospace()
                                .size(9.0)
                                .color(MUTED),
                        )
                        .truncate(),
                    );
                    if task.is_template() {
                        ui.label(
                            RichText::new(format!(
                                "ШАБЛОН · {} сценариев · {} раскрытых шагов",
                                task.scenarios.len(),
                                resolved_task
                                    .as_ref()
                                    .map(|resolved| task_action_steps(resolved).len())
                                    .unwrap_or_default()
                            ))
                            .strong()
                            .size(9.0)
                            .color(PURPLE),
                        );
                    }
                    ui.add_space(10.0);
                    let task_description = if task.description.trim().is_empty() {
                        "Подробное описание для этого сценария пока не задано."
                    } else {
                        &task.description
                    };
                    ui.add(
                        egui::Label::new(
                            RichText::new(task_description)
                            .size(10.0)
                            .color(text(self.dark)),
                        )
                        .truncate(),
                    )
                    .on_hover_text(task_description);
                    ui.add_space(14.0);

                    if let Some(error) = &resolution_error {
                        error_box(ui, error, self.dark);
                        ui.add_space(14.0);
                    }

                    if resolved_task.as_ref().is_some_and(|resolved| {
                        task_contains_action(resolved, &|action| {
                            matches!(action, Action::BambuStudioRelease(_))
                        })
                    })
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
                                    .truncate(),
                                )
                                .on_hover_text(summary);
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
                                        let report_line = format!(
                                            "{}: {}",
                                            step.step_name, result
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&report_line)
                                                .size(9.0)
                                                .color(text(self.dark)),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(report_line);
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
                                .truncate(),
                            )
                            .on_hover_text(&group.description);
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
                                        .truncate(),
                                    )
                                    .on_hover_text(summary);
                                });
                            }
                        }
                    } else {
                        section_label(ui, "ВЫБРАННЫЙ УЗЕЛ");
                        if let Some(node) = self.selected_node.as_deref().and_then(|node_id| {
                            resolved_task
                                .as_ref()
                                .and_then(|resolved| resolved.graph.as_ref())
                                .and_then(|graph| graph_node(graph, node_id))
                        }) {
                            match node {
                                GraphNode::Action(node) => paint_step_inspector(
                                    ui,
                                    &node.step,
                                    preview_options.as_ref(),
                                    self.dark,
                                ),
                                GraphNode::ForEach(node) => graph_control_summary(
                                    ui,
                                    "FOR EACH",
                                    &node.id,
                                    self.dark,
                                ),
                                GraphNode::If(node) => {
                                    graph_control_summary(ui, "IF", &node.id, self.dark)
                                }
                                GraphNode::Switch(node) => {
                                    graph_control_summary(ui, "SWITCH", &node.id, self.dark)
                                }
                                GraphNode::Join(node) => {
                                    graph_control_summary(ui, "JOIN", &node.id, self.dark)
                                }
                            }
                        } else if let Some(step) = self.selected_step.and_then(|step_index| {
                            resolved_task
                                .as_ref()
                                .and_then(|resolved| task_action_steps(resolved).get(step_index).copied())
                        }) {
                            paint_step_inspector(ui, step, preview_options.as_ref(), self.dark);
                        }
                    }
                });
            });
    }

    fn composer_inspector(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut remove_node = None;
        let mut remove_optional_scope = None;
        let mut remove_switch_case = None;
        let mut open_graph_attach = None;
        let mut incoming_edge_changes = Vec::new();
        let selected_node = self.selected_node.clone();
        let selected_path = self.selected_project_scenario.clone();
        {
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
            changed |= ui
                .add(egui::TextEdit::singleline(&mut task.name).desired_width(ui.available_width()))
                .changed();
            ui.label(RichText::new("ID").size(9.0).color(MUTED));
            changed |= ui
                .add(egui::TextEdit::singleline(&mut task.id).desired_width(ui.available_width()))
                .changed();
            ui.label(RichText::new("Описание").size(9.0).color(MUTED));
            changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut task.description)
                        .desired_rows(3)
                        .desired_width(ui.available_width()),
                )
                .changed();
            ui.add_space(12.0);

            section_label(ui, "ВЫБРАННЫЙ БЛОК");
            if let (Some(graph_snapshot), Some(node_id)) =
                (task.graph.clone(), selected_node.as_deref())
            {
                if graph_node(&graph_snapshot, node_id).is_none() {
                    ui.label(
                        RichText::new("Выбранный узел больше не существует.")
                            .size(9.0)
                            .color(ORANGE),
                    );
                } else {
                    ui.horizontal(|ui| {
                        if ui.button("Удалить").clicked() {
                            remove_node = Some(node_id.to_owned());
                        }
                    });
                    ui.add_space(8.0);
                    let action_options = match graph_node(&graph_snapshot, node_id) {
                        Some(GraphNode::Action(node)) => {
                            let definition = definition_for_action(&node.step.action);
                            graph_input_fields(&definition.input_schema)
                                .into_iter()
                                .map(|(target, field)| {
                                    (
                                        target,
                                        graph_binding_options(&graph_snapshot, node_id, &field),
                                    )
                                })
                                .collect::<BTreeMap<_, _>>()
                        }
                        _ => BTreeMap::new(),
                    };
                    let condition_fields = graph_condition_fields(&graph_snapshot, node_id);
                    let control_binding_options = graph_binding_options(
                        &graph_snapshot,
                        node_id,
                        &FieldSchema::optional(ContextType::Any).nullable(),
                    );
                    let array_options = graph_array_options(&graph_snapshot, node_id);
                    let selected_scope = graph_visual_model(&graph_snapshot)
                        .0
                        .into_iter()
                        .find(|node| node.id == node_id)
                        .and_then(|node| node.scope);
                    let Some(graph) = task.graph.as_mut() else {
                        return;
                    };
                    let Some(node) = graph_node_mut(graph, node_id) else {
                        return;
                    };
                    match node {
                        GraphNode::Action(node) => {
                            changed |=
                                paint_graph_action_editor(ui, node, &action_options, self.dark);
                            ui.add_space(12.0);
                            changed |= paint_composer_conditions(
                                ui,
                                &mut node.step,
                                &condition_fields,
                                self.dark,
                            );
                            ui.add_space(12.0);
                            section_label(ui, "ВЫХОДНОЙ КОНТЕКСТ");
                            for line in schema_context_lines(
                                &definition_for_action(&node.step.action).output_schema,
                            ) {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(line).monospace().size(8.0).color(PURPLE),
                                    )
                                    .truncate(),
                                );
                            }
                        }
                        GraphNode::ForEach(node) => {
                            ui.label(RichText::new("ID блока").size(9.0).color(MUTED));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&node.id).monospace().size(9.0).color(PURPLE),
                                )
                                .truncate(),
                            );
                            ui.label(RichText::new("Коллекция").size(9.0).color(MUTED));
                            egui::ComboBox::from_id_salt(("foreach-collection", &node.id))
                                .selected_text(binding_label(&node.collection))
                                .truncate()
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for (label, binding, alias, _) in &array_options {
                                        if ui
                                            .selectable_label(node.collection == *binding, label)
                                            .clicked()
                                        {
                                            node.collection = binding.clone();
                                            node.item_alias = first_free_loop_alias(
                                                &graph_snapshot,
                                                alias,
                                                Some(&node.id),
                                            );
                                            changed = true;
                                            ui.close();
                                        }
                                    }
                                });
                            if array_options.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "В доминирующем контексте нет типизированного массива.",
                                    )
                                    .size(8.0)
                                    .color(ORANGE),
                                );
                            }
                            ui.label(
                                RichText::new("Имя текущего элемента")
                                    .size(9.0)
                                    .color(MUTED),
                            );
                            changed |= ui
                                .add(
                                    egui::TextEdit::singleline(&mut node.item_alias)
                                        .desired_width(ui.available_width()),
                                )
                                .changed();
                            let mut publish_index = node.index_alias.is_some();
                            if ui
                                .checkbox(&mut publish_index, "Публиковать индекс итерации")
                                .changed()
                            {
                                node.index_alias = publish_index.then(|| {
                                    first_free_loop_alias(
                                        &graph_snapshot,
                                        &format!("{}_index", node.item_alias),
                                        Some(&node.id),
                                    )
                                });
                                changed = true;
                            }
                            if let Some(index_alias) = &mut node.index_alias {
                                ui.label(
                                    RichText::new("Имя индекса итерации").size(9.0).color(MUTED),
                                );
                                changed |= ui
                                    .add(
                                        egui::TextEdit::singleline(index_alias)
                                            .desired_width(ui.available_width()),
                                    )
                                    .changed();
                            }
                            ui.label(RichText::new("Параллельность").size(9.0).color(MUTED));
                            changed |= ui
                                .add(egui::DragValue::new(&mut node.concurrency).range(1..=64))
                                .changed();
                            let mut continue_on_error =
                                matches!(node.on_error, LoopFailurePolicy::Continue);
                            if ui
                                .checkbox(&mut continue_on_error, "Продолжать после ошибки")
                                .changed()
                            {
                                node.on_error = if continue_on_error {
                                    LoopFailurePolicy::Continue
                                } else {
                                    LoopFailurePolicy::Stop
                                };
                                changed = true;
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new("＋ Для каждого item")
                                            .fill(if node.body.nodes.is_empty() {
                                                PURPLE
                                            } else {
                                                panel(self.dark)
                                            }),
                                    )
                                    .on_hover_text(
                                        "Этот блок выполнится отдельно для каждого элемента коллекции.",
                                    )
                                    .clicked()
                                {
                                    open_graph_attach = Some(ComposerGraphAttach::NestedStart {
                                        scope: ComposerGraphNestedScope::ForEachBody {
                                            owner_id: node.id.clone(),
                                        },
                                    });
                                }
                                if ui
                                    .add_enabled(
                                        !node.body.nodes.is_empty(),
                                        egui::Button::new("＋ После цикла"),
                                    )
                                    .on_disabled_hover_text(
                                        "Сначала добавьте хотя бы один блок для каждого item.",
                                    )
                                    .on_hover_text(
                                        "Этот блок выполнится один раз после завершения всех итераций.",
                                    )
                                    .clicked()
                                {
                                    open_graph_attach = Some(selected_scope.clone().map_or_else(
                                        || ComposerGraphAttach::RootAfter {
                                            node_id: node.id.clone(),
                                        },
                                        |scope| ComposerGraphAttach::NestedAfter {
                                            scope,
                                            node_id: node.id.clone(),
                                        },
                                    ));
                                }
                            });
                            ui.label(
                                RichText::new(if node.body.nodes.is_empty() {
                                    "Цикл пока пуст: добавьте действие через «Для каждого item»."
                                        .to_owned()
                                } else {
                                    format!(
                                        "В итерации: {} блоков. «После цикла» — отдельный однократный выход.",
                                        node.body.nodes.len()
                                    )
                                })
                                .size(8.0)
                                .color(if node.body.nodes.is_empty() { ORANGE } else { MUTED }),
                            );
                            ui.add_space(12.0);
                            section_label(ui, "КОНТЕКСТ ИТЕРАЦИИ");
                            if let Some(item_type) = graph_loop_item_type(&graph_snapshot, &node.id)
                            {
                                let mut lines = Vec::new();
                                collect_context_type_lines(
                                    &item_type,
                                    &node.item_alias,
                                    &mut lines,
                                );
                                for line in lines {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(line).monospace().size(8.0).color(PURPLE),
                                        )
                                        .truncate(),
                                    );
                                }
                            }
                        }
                        GraphNode::If(node) => {
                            graph_control_summary(ui, "IF", &node.id, self.dark);
                            let mut editable =
                                composer_condition_rule(&node.condition).filter(|rule| {
                                    composer_condition_rule_supported(rule, &condition_fields)
                                });
                            if let Some(rule) = editable.as_mut() {
                                if paint_composer_condition_rule_editor(
                                    ui,
                                    &format!("if-{}", node.id),
                                    rule,
                                    &condition_fields,
                                    self.dark,
                                ) {
                                    node.condition = build_composer_condition_rule(rule);
                                    changed = true;
                                }
                            } else if let ExpressionV1::Literal {
                                value: ExpressionValue::Bool(value),
                            } = &mut node.condition
                            {
                                changed |= ui.checkbox(value, "Литеральное условие").changed();
                            } else {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(
                                            "Условие использует выражение, которое визуальный редактор не поддерживает. Оно сохранено без изменений.",
                                        )
                                        .size(8.0)
                                        .color(ORANGE),
                                    )
                                    .wrap(),
                                );
                                Frame::new()
                                    .fill(code_surface(self.dark))
                                    .corner_radius(6)
                                    .inner_margin(Margin::same(7))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(condition_read_only_summary(
                                                    &node.condition,
                                                ))
                                                .monospace()
                                                .size(8.0)
                                                .color(text(self.dark)),
                                            )
                                            .truncate(),
                                        );
                                    });
                                if ui
                                    .button("Заменить условие типизированным правилом")
                                    .clicked()
                                {
                                    node.condition = default_graph_if_condition(&condition_fields);
                                    changed = true;
                                }
                            }
                            ui.horizontal(|ui| {
                                if ui.button("＋ Ветка then").clicked() {
                                    open_graph_attach = Some(ComposerGraphAttach::NestedStart {
                                        scope: ComposerGraphNestedScope::IfThen {
                                            owner_id: node.id.clone(),
                                        },
                                    });
                                }
                                if ui.button("＋ Ветка else").clicked() {
                                    open_graph_attach = Some(ComposerGraphAttach::NestedStart {
                                        scope: ComposerGraphNestedScope::IfElse {
                                            owner_id: node.id.clone(),
                                        },
                                    });
                                }
                            });
                            if let Some(else_graph) = node.else_graph.as_deref() {
                                let empty = graph_scope_is_empty(else_graph);
                                if ui
                                    .add_enabled(empty, egui::Button::new("Удалить ветку Иначе"))
                                    .on_disabled_hover_text(
                                        "Сначала удалите или перенесите блоки из ветки «Иначе».",
                                    )
                                    .clicked()
                                {
                                    remove_optional_scope =
                                        Some(ComposerGraphNestedScope::IfElse {
                                            owner_id: node.id.clone(),
                                        });
                                }
                            }
                        }
                        GraphNode::Switch(node) => {
                            graph_control_summary(ui, "SWITCH", &node.id, self.dark);
                            ui.label(RichText::new("Selector").size(9.0).color(MUTED));
                            egui::ComboBox::from_id_salt(("switch-selector", &node.id))
                                .selected_text(binding_label(&node.selector))
                                .truncate()
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(
                                            matches!(&node.selector, Binding::Literal { .. }),
                                            "Литеральное значение",
                                        )
                                        .clicked()
                                    {
                                        node.selector = Binding::literal("");
                                        changed = true;
                                        ui.close();
                                    }
                                    for option in control_binding_options.iter().filter(|option| {
                                        GraphSwitchScalarKind::from_context_type(&option.value_type)
                                            .is_some()
                                    }) {
                                        if ui
                                            .selectable_label(
                                                node.selector == option.binding,
                                                &option.label,
                                            )
                                            .clicked()
                                        {
                                            node.selector = option.binding.clone();
                                            changed = true;
                                            ui.close();
                                        }
                                    }
                                });
                            if let Binding::Literal { value } = &mut node.selector {
                                if let Some(mut kind) = GraphSwitchScalarKind::from_value(value) {
                                    let before = kind;
                                    egui::ComboBox::from_id_salt((
                                        "switch-selector-type",
                                        &node.id,
                                    ))
                                    .selected_text(kind.label())
                                    .truncate()
                                    .width(ui.available_width())
                                    .show_ui(ui, |ui| {
                                        for candidate in GraphSwitchScalarKind::ALL {
                                            ui.selectable_value(
                                                &mut kind,
                                                candidate,
                                                candidate.label(),
                                            );
                                        }
                                    });
                                    if kind != before {
                                        *value = kind.default_value(0);
                                        changed = true;
                                    }
                                } else {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(
                                                "Составной selector сохранён без автоматического преобразования.",
                                            )
                                            .size(8.0)
                                            .color(ORANGE),
                                        )
                                        .wrap(),
                                    );
                                }
                                changed |= paint_graph_literal_editor(
                                    ui,
                                    (&node.id, "switch-selector-literal"),
                                    value,
                                );
                            }
                            let selector_contract = graph_switch_selector_contract(
                                &node.selector,
                                &control_binding_options,
                            );
                            let selector_kind = selector_contract.map(|(kind, _)| kind);
                            let selector_nullable =
                                selector_contract.is_some_and(|(_, nullable)| nullable);
                            let null_case_exists = node
                                .cases
                                .iter()
                                .any(|case| case.values.iter().any(serde_json::Value::is_null));
                            let next_scalar_value = selector_kind
                                .and_then(|kind| first_free_switch_case_value(kind, &node.cases));
                            let scalar_slots_full = matches!(
                                selector_kind,
                                Some(GraphSwitchScalarKind::Boolean | GraphSwitchScalarKind::Null)
                            ) && next_scalar_value.is_none();
                            let mut used_case_values = node
                                .cases
                                .iter()
                                .flat_map(|case| case.values.iter().cloned())
                                .collect::<Vec<_>>();
                            let mut case_action = None;
                            let case_count = node.cases.len();
                            for (index, case) in node.cases.iter_mut().enumerate() {
                                ui.add_space(6.0);
                                if let Some(action) = paint_switch_case_header(
                                    ui,
                                    &case.id,
                                    index,
                                    case_count,
                                    graph_scope_is_empty(&case.graph),
                                ) {
                                    if action == SwitchCaseHeaderAction::OpenBranch {
                                        open_graph_attach =
                                            Some(ComposerGraphAttach::NestedStart {
                                                scope: ComposerGraphNestedScope::SwitchCase {
                                                    owner_id: node.id.clone(),
                                                    case_id: case.id.clone(),
                                                },
                                            });
                                    } else {
                                        case_action = Some((action, index));
                                    }
                                }
                                let mut remove_value = None;
                                let value_count = case.values.len();
                                for (value_index, value) in case.values.iter_mut().enumerate() {
                                    let (value_changed, remove) = paint_switch_case_value_row(
                                        ui,
                                        (&node.id, &case.id),
                                        value_index,
                                        value_count,
                                        value,
                                        selector_contract,
                                        &mut used_case_values,
                                    );
                                    changed |= value_changed;
                                    if remove {
                                        remove_value = Some(value_index);
                                    }
                                }
                                if let Some(value_index) = remove_value {
                                    case.values.remove(value_index);
                                    changed = true;
                                }
                                if ui
                                    .add_enabled(
                                        first_free_switch_value_from_used(
                                            selector_kind.unwrap_or(GraphSwitchScalarKind::String),
                                            &used_case_values,
                                        )
                                        .is_some(),
                                        egui::Button::new("＋ Значение case"),
                                    )
                                    .clicked()
                                {
                                    let kind =
                                        selector_kind.unwrap_or(GraphSwitchScalarKind::String);
                                    if let Some(value) =
                                        first_free_switch_value_from_used(kind, &used_case_values)
                                    {
                                        case.values.push(value.clone());
                                        used_case_values.push(value);
                                        changed = true;
                                    }
                                }
                                if case.values.is_empty() {
                                    ui.label(
                                        RichText::new(
                                            "Case должен содержать хотя бы одно значение",
                                        )
                                        .size(8.0)
                                        .color(ORANGE),
                                    );
                                }
                            }
                            if let Some((action, index)) = case_action {
                                match action {
                                    SwitchCaseHeaderAction::MoveUp => {
                                        node.cases.swap(index, index - 1)
                                    }
                                    SwitchCaseHeaderAction::MoveDown => {
                                        node.cases.swap(index, index + 1)
                                    }
                                    SwitchCaseHeaderAction::Remove => {
                                        remove_switch_case =
                                            Some((node.id.clone(), node.cases[index].id.clone()));
                                    }
                                    SwitchCaseHeaderAction::OpenBranch => {
                                        unreachable!("open branch is handled before mutating cases")
                                    }
                                }
                                changed |= action != SwitchCaseHeaderAction::Remove;
                            }
                            if ui
                                .add_enabled(!scalar_slots_full, egui::Button::new("＋ Case"))
                                .clicked()
                            {
                                let kind = selector_kind.unwrap_or(GraphSwitchScalarKind::String);
                                let case_id = first_free_switch_case_id(&node.cases);
                                if let Some(value) = first_free_switch_case_value(kind, &node.cases)
                                {
                                    node.cases.push(SwitchCase {
                                        id: case_id,
                                        values: vec![value],
                                        graph: Box::new(WorkflowGraph::default()),
                                    });
                                    changed = true;
                                }
                            }
                            if selector_nullable
                                && !null_case_exists
                                && ui.button("＋ Case со значением null").clicked()
                            {
                                let case_id = first_free_switch_case_id(&node.cases);
                                node.cases.push(SwitchCase {
                                    id: case_id,
                                    values: vec![serde_json::Value::Null],
                                    graph: Box::new(WorkflowGraph::default()),
                                });
                                changed = true;
                            }
                            if ui.button("＋ Default ветка").clicked() {
                                open_graph_attach = Some(ComposerGraphAttach::NestedStart {
                                    scope: ComposerGraphNestedScope::SwitchDefault {
                                        owner_id: node.id.clone(),
                                    },
                                });
                            }
                            if let Some(default) = node.default.as_deref() {
                                let empty = graph_scope_is_empty(default);
                                if ui
                                    .add_enabled(
                                        empty,
                                        egui::Button::new("Удалить ветку По умолчанию"),
                                    )
                                    .on_disabled_hover_text(
                                        "Сначала удалите или перенесите блоки из ветки «По умолчанию».",
                                    )
                                    .clicked()
                                {
                                    remove_optional_scope = Some(
                                        ComposerGraphNestedScope::SwitchDefault {
                                            owner_id: node.id.clone(),
                                        },
                                    );
                                }
                            }
                        }
                        GraphNode::Join(node) => {
                            graph_control_summary(ui, "JOIN", &node.id, self.dark);
                            egui::ComboBox::from_id_salt(("join-mode", &node.id))
                                .selected_text(format!("{:?}", node.mode))
                                .truncate()
                                .show_ui(ui, |ui| {
                                    changed |= ui
                                        .selectable_value(&mut node.mode, JoinMode::All, "Все")
                                        .changed();
                                    changed |= ui
                                        .selectable_value(&mut node.mode, JoinMode::Any, "Любой")
                                        .changed();
                                    changed |= ui
                                        .selectable_value(
                                            &mut node.mode,
                                            JoinMode::FirstSuccessful,
                                            "Первый успешный",
                                        )
                                        .changed();
                                });
                            ui.label(RichText::new("Входящие ветви").size(9.0).color(MUTED));
                            if let Some(scope) = graph_local_scope(&graph_snapshot, &node.id) {
                                for source in scope.nodes.iter().filter(|source| {
                                    source.id() != node.id
                                        && !matches!(source, GraphNode::Join(other) if other.id == node.id)
                                }) {
                                    let existing = scope
                                        .edges
                                        .iter()
                                        .find(|edge| {
                                            edge.from.node == source.id()
                                                && edge.to.node == node.id
                                        })
                                        .map(|edge| edge.from.port.clone());
                                    if let Some(port) =
                                        paint_join_source_row(ui, &node.id, source, existing)
                                    {
                                        incoming_edge_changes.push((
                                            node.id.clone(),
                                            source.id().to_owned(),
                                            port,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                ui.label(
                    RichText::new(if task.graph.is_some() {
                        "Выберите узел на канвасе или добавьте его из палитры слева."
                    } else {
                        "Legacy Task.steps доступен только импортеру; сохраните сценарий как WorkflowGraph v3."
                    })
                        .size(9.0)
                        .color(MUTED),
                );
            }
        }
        if changed {
            self.invalidate_plan();
        }
        if let Some(node_id) = remove_node {
            self.remove_composer_node(&node_id);
        }
        if remove_optional_scope.is_some() || remove_switch_case.is_some() {
            let selected_path = self.selected_project_scenario.clone();
            if let Some(graph) = self
                .custom_project
                .as_mut()
                .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
                .and_then(|task| task.graph.as_mut())
            {
                let result = if let Some(scope) = remove_optional_scope {
                    graph_remove_optional_scope(graph, &scope)
                } else if let Some((switch_id, case_id)) = remove_switch_case {
                    graph_remove_switch_case(graph, &switch_id, &case_id)
                } else {
                    Ok(false)
                };
                match result {
                    Ok(true) => self.invalidate_plan(),
                    Ok(false) => {}
                    Err(error) => self.file_message = Some((true, error)),
                }
            }
        }
        if let Some(attach) = open_graph_attach {
            self.open_graph_block_picker(attach);
        }
        if !incoming_edge_changes.is_empty() {
            let selected_path = self.selected_project_scenario.clone();
            if let Some(graph) = self
                .custom_project
                .as_mut()
                .and_then(|project| project.scenario_mut(selected_path.as_deref()?))
                .and_then(|task| task.graph.as_mut())
            {
                for (target, source, port) in incoming_edge_changes {
                    if let Err(error) = graph_set_incoming_edge(graph, &target, &source, port) {
                        self.file_message = Some((true, error));
                    }
                }
                self.invalidate_plan();
            }
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
            .custom_project
            .as_ref()
            .ok_or_else(|| "проект не выбран".to_owned())
            .and_then(validate_project);
        match &validation {
            Ok(()) => {
                ui.label(
                    RichText::new("Сценарий корректен и готов к сохранению.")
                        .size(9.0)
                        .color(CYAN),
                );
            }
            Err(error) => {
                section_label(ui, "НУЖНО ИСПРАВИТЬ");
                error_box(ui, error, self.dark);
            }
        }
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
        let mut request_github_authorization = false;
        if let Some(error) = &self.plan_error {
            ui.add_space(8.0);
            error_box(ui, error, self.dark);
        } else if let Some(report) = &self.report {
            paint_composer_run_report(
                ui,
                report,
                self.selected_step,
                self.report_applied,
                self.dark,
            );
            if github_report_needs_authorization(report) {
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        !self.github_picker.authorizing && !self.running,
                        egui::Button::new("Войти через GitHub и повторить")
                            .min_size(Vec2::new(ui.available_width(), 32.0)),
                    )
                    .clicked()
                {
                    request_github_authorization = true;
                }
                if self.github_picker.authorizing
                    && matches!(
                        self.github_picker.authorization_intent,
                        GithubAuthorizationIntent::RetryScenario
                    )
                {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Ожидаю подтверждения входа в браузере…")
                                .size(8.0)
                                .color(MUTED),
                        );
                    });
                } else if matches!(
                    self.github_picker.authorization_intent,
                    GithubAuthorizationIntent::RetryScenario
                ) {
                    if let Some(error) = &self.github_picker.error {
                        error_box(ui, error, self.dark);
                    }
                }
            }
        }
        if request_github_authorization {
            self.start_github_authorization(ui.ctx(), GithubAuthorizationIntent::RetryScenario);
        }
    }

    fn graph_composer_canvas(
        &mut self,
        ui: &mut egui::Ui,
        task: &Task,
        graph: &WorkflowGraph,
        canvas: &ComposerCanvas,
        editable: bool,
    ) {
        let (nodes, edges) = graph_visual_model(graph);
        let node_size = Vec2::new(232.0, 116.0);
        let world_bounds = graph_canvas_world_bounds(canvas, node_size);
        let readonly_view_key = task.id.clone();
        let mut view = if editable {
            canvas.view.sanitized()
        } else {
            self.readonly_canvas_views
                .get(&readonly_view_key)
                .copied()
                .unwrap_or(canvas.view)
                .sanitized()
        };

        let mut zoom_out = false;
        let mut reset_view = false;
        let mut zoom_in = false;
        let mut fit_view = false;
        Frame::new()
            .fill(panel(self.dark))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("КАНВАС")
                            .strong()
                            .size(9.0)
                            .color(MUTED),
                    )
                    .on_hover_text(
                        "Перетаскивание фона или средней кнопкой — перемещение. ⌘/Ctrl + колесо или pinch — масштаб.",
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        fit_view = ui
                            .small_button("Вписать")
                            .on_hover_text("Показать весь граф")
                            .clicked();
                        zoom_in = ui
                            .small_button("+")
                            .on_hover_text("Увеличить масштаб")
                            .clicked();
                        reset_view = ui
                            .small_button(format!("{:.0}%", view.zoom * 100.0))
                            .on_hover_text("Сбросить перемещение и масштаб")
                            .clicked();
                        zoom_out = ui
                            .small_button("−")
                            .on_hover_text("Уменьшить масштаб")
                            .clicked();
                    });
                });
            });
        ui.add_space(4.0);

        let viewport = ui.available_rect_before_wrap();
        if reset_view {
            view = CanvasView::default();
        } else if fit_view {
            view = CanvasView::fit(world_bounds, viewport, CANVAS_FIT_PADDING);
        } else if zoom_out {
            view.zoom_about(viewport, viewport.center(), 1.0 / CANVAS_ZOOM_STEP);
        } else if zoom_in {
            view.zoom_about(viewport, viewport.center(), CANVAS_ZOOM_STEP);
        }

        let mut scene_rect = view.visible_world_rect(viewport);
        let gestures_enabled =
            self.block_picker_parent.is_none() && !self.github_picker.open && !self.confirm_run;
        // A dedicated interaction inside the transformed layer owns blank
        // canvas drags. It is registered before the smaller card hit zones,
        // so cards reliably win without the full-scene parent racing them.
        let mut background_drag = None;
        let mut space_pan_delta = Vec2::ZERO;
        let mut child_consumed_drag = false;
        let scene = egui::Scene::new()
            .zoom_range(CANVAS_MIN_ZOOM..=CANVAS_MAX_ZOOM)
            .max_inner_size(Vec2::splat(100_000.0))
            .sense(Sense::hover())
            .drag_pan_buttons(egui::DragPanButtons::empty());
        scene.show(ui, &mut scene_rect, |ui| {
            let painter = ui.painter().clone();
            let bounds = ui.clip_rect();
            background_drag = Some(ui.interact(
                bounds,
                Id::new(("graph-canvas-background", task.id.as_str())),
                if gestures_enabled {
                    Sense::click_and_drag()
                } else {
                    Sense::hover()
                },
            ));
            paint_grid(&painter, bounds, self.dark);
            let space_down = ui.input(|input| input.key_down(egui::Key::Space));
            let positions = canvas
                .positions
                .iter()
                .map(|(id, point)| (id.clone(), Pos2::new(point.x, point.y)))
                .collect::<BTreeMap<_, _>>();
            for edge in &edges {
                let (Some(from), Some(to)) = (positions.get(&edge.from), positions.get(&edge.to))
                else {
                    continue;
                };
                paint_graph_connector(
                    &painter,
                    *from + Vec2::new(node_size.x, node_size.y * 0.5),
                    *to + Vec2::new(0.0, node_size.y * 0.5),
                    edge.kind,
                    edge.port.as_ref(),
                );
            }

            if let Some(position) = positions.get("start") {
                let rect = Rect::from_min_size(*position, node_size);
                let drag = ui.interact(
                    rect,
                    Id::new(("graph-start", task.id.as_str())),
                    Sense::click_and_drag(),
                );
                if drag.dragged_by(egui::PointerButton::Middle) {
                    child_consumed_drag = true;
                    space_pan_delta += drag.drag_delta();
                } else if drag.dragged_by(egui::PointerButton::Primary) {
                    child_consumed_drag = true;
                    if space_down || !editable {
                        space_pan_delta += drag.drag_delta();
                    } else {
                        self.drag_composer_node(&task.id, "start", drag.drag_delta());
                    }
                }
                painter.rect_filled(rect, 13.0, panel(self.dark));
                painter.rect_stroke(rect, 13.0, Stroke::new(2.0, CYAN), StrokeKind::Inside);
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
                let plus_rect = Rect::from_center_size(
                    Pos2::new(rect.right() - 20.0, rect.center().y),
                    Vec2::splat(30.0),
                );
                if editable {
                    paint_graph_plus(&painter, plus_rect, "+");
                }
                if editable
                    && ui
                        .interact(
                            plus_rect,
                            Id::new(("graph-start-plus", task.id.as_str())),
                            Sense::click(),
                        )
                        .clicked_by(egui::PointerButton::Primary)
                {
                    self.open_graph_block_picker(ComposerGraphAttach::RootStart);
                }
            }

            for (index, node) in nodes.iter().enumerate() {
                let Some(position) = positions.get(&node.id) else {
                    continue;
                };
                let rect = Rect::from_min_size(*position, node_size);
                let interaction = ui.interact(
                    rect,
                    Id::new(("graph-node", task.id.as_str(), node.id.as_str())),
                    Sense::click_and_drag(),
                );
                if interaction.clicked_by(egui::PointerButton::Primary) {
                    self.selected_node = Some(node.id.clone());
                    self.selected_step = None;
                }
                if interaction.dragged_by(egui::PointerButton::Middle) {
                    child_consumed_drag = true;
                    space_pan_delta += interaction.drag_delta();
                } else if interaction.dragged_by(egui::PointerButton::Primary) {
                    child_consumed_drag = true;
                    if space_down || !editable {
                        space_pan_delta += interaction.drag_delta();
                    } else {
                        self.drag_composer_node(&task.id, &node.id, interaction.drag_delta());
                    }
                }
                let status = self.report.as_ref().and_then(|report| {
                    report
                        .steps
                        .iter()
                        .find(|step| step.step_id == node.id)
                        .map(|step| &step.status)
                });
                match &node.card {
                    ComposerGraphCard::Action(step) => paint_step_node(
                        &painter,
                        rect,
                        step,
                        index,
                        self.selected_node.as_deref() == Some(node.id.as_str()),
                        status,
                        self.dark,
                    ),
                    card => paint_graph_control_node(
                        &painter,
                        rect,
                        &node.id,
                        card,
                        self.selected_node.as_deref() == Some(node.id.as_str()),
                        status,
                        self.dark,
                    ),
                }

                if !editable {
                    continue;
                }

                match &node.card {
                    ComposerGraphCard::ForEach { body_empty, .. } => {
                        let body_rect = Rect::from_center_size(
                            Pos2::new(rect.right() - 18.0, rect.top() + 38.0),
                            Vec2::splat(28.0),
                        );
                        let completed_rect = Rect::from_center_size(
                            Pos2::new(rect.right() - 18.0, rect.bottom() - 28.0),
                            Vec2::splat(28.0),
                        );
                        painter.text(
                            body_rect.left_center() - Vec2::new(8.0, 0.0),
                            Align2::RIGHT_CENTER,
                            "для item",
                            FontId::proportional(8.0),
                            CYAN,
                        );
                        painter.text(
                            completed_rect.left_center() - Vec2::new(8.0, 0.0),
                            Align2::RIGHT_CENTER,
                            "после",
                            FontId::proportional(8.0),
                            if *body_empty { MUTED } else { PURPLE },
                        );
                        paint_graph_plus(&painter, body_rect, "∀");
                        if *body_empty {
                            painter.circle_filled(completed_rect.center(), 12.0, panel(self.dark));
                            painter.circle_stroke(
                                completed_rect.center(),
                                12.0,
                                Stroke::new(1.0, MUTED),
                            );
                            painter.text(
                                completed_rect.center(),
                                Align2::CENTER_CENTER,
                                "–",
                                FontId::proportional(16.0),
                                MUTED,
                            );
                        } else {
                            paint_graph_plus(&painter, completed_rect, "+");
                        }
                        if ui
                            .interact(
                                body_rect,
                                Id::new(("graph-loop-body-plus", node.id.as_str())),
                                Sense::click(),
                            )
                            .on_hover_text("Добавить в тело: один item на итерацию")
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            self.open_graph_block_picker(ComposerGraphAttach::NestedStart {
                                scope: ComposerGraphNestedScope::ForEachBody {
                                    owner_id: node.id.clone(),
                                },
                            });
                        }
                        let completed = ui.interact(
                            completed_rect,
                            Id::new(("graph-loop-completed-plus", node.id.as_str())),
                            if *body_empty {
                                Sense::hover()
                            } else {
                                Sense::click()
                            },
                        );
                        let completed = if *body_empty {
                            completed.on_hover_text(
                                "Сначала добавьте хотя бы один блок для каждого item.",
                            )
                        } else {
                            completed
                                .on_hover_text("Добавить один блок после завершения всех итераций")
                        };
                        if !*body_empty && completed.clicked_by(egui::PointerButton::Primary) {
                            let attach = node.scope.clone().map_or_else(
                                || ComposerGraphAttach::RootAfter {
                                    node_id: node.id.clone(),
                                },
                                |scope| ComposerGraphAttach::NestedAfter {
                                    scope,
                                    node_id: node.id.clone(),
                                },
                            );
                            self.open_graph_block_picker(attach);
                        }
                    }
                    _ => {
                        let plus_rect = Rect::from_center_size(
                            Pos2::new(rect.right() - 18.0, rect.center().y),
                            Vec2::splat(28.0),
                        );
                        paint_graph_plus(&painter, plus_rect, "+");
                        if ui
                            .interact(
                                plus_rect,
                                Id::new(("graph-node-plus", node.id.as_str())),
                                Sense::click(),
                            )
                            .clicked_by(egui::PointerButton::Primary)
                        {
                            let attach = node.scope.clone().map_or_else(
                                || ComposerGraphAttach::RootAfter {
                                    node_id: node.id.clone(),
                                },
                                |scope| ComposerGraphAttach::NestedAfter {
                                    scope,
                                    node_id: node.id.clone(),
                                },
                            );
                            self.open_graph_block_picker(attach);
                        }
                    }
                }
            }

            painter.text(
                Pos2::new(80.0, 92.0),
                Align2::LEFT_TOP,
                "WORKFLOW GRAPH",
                FontId::proportional(10.0),
                MUTED,
            );
            painter.text(
                Pos2::new(80.0, 112.0),
                Align2::LEFT_TOP,
                &task.name,
                FontId::proportional(26.0),
                text(self.dark),
            );
            painter.text(
                Pos2::new(80.0, 150.0),
                Align2::LEFT_TOP,
                format!("{} узлов · {} связей", nodes.len(), edges.len()),
                FontId::proportional(10.0),
                MUTED,
            );
        });
        if space_pan_delta != Vec2::ZERO {
            scene_rect = scene_rect.translate(-space_pan_delta);
        } else if let Some(background_drag) = background_drag.filter(|background_drag| {
            gestures_enabled
                && !child_consumed_drag
                && (background_drag.dragged_by(egui::PointerButton::Primary)
                    || background_drag.dragged_by(egui::PointerButton::Middle))
        }) {
            // This response lives in the Scene layer, so egui has already
            // converted its per-frame drag delta to world coordinates.
            scene_rect = scene_rect.translate(-background_drag.drag_delta());
        }
        let updated_view = CanvasView::from_visible_world_rect(scene_rect, viewport);
        if editable {
            self.set_composer_canvas_view(&task.id, updated_view);
        } else {
            self.readonly_canvas_views
                .insert(readonly_view_key, updated_view);
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
                if is_composer {
                    if let (Some(graph), Some(canvas)) = (&task.graph, composer_canvas.as_ref()) {
                        self.graph_composer_canvas(ui, &task, graph, canvas, true);
                        return;
                    }
                    error_box(
                        ui,
                        "Сценарий не импортирован в WorkflowGraph v3; Task.steps нельзя редактировать на канвасе.",
                        self.dark,
                    );
                    return;
                }
                if let Some(graph) = &task.graph {
                    let canvas = default_graph_canvas(graph);
                    self.graph_composer_canvas(ui, &task, graph, &canvas, false);
                    return;
                }
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
                                &task,
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
            self.start_github_authorization(ctx, GithubAuthorizationIntent::RepositoryPicker);
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
    let step = default_step(ActionKind::GithubListRepositories, "list-repositories")
        .expect("GitHub repository discovery is a graph action");
    let graph = WorkflowGraph {
        entries: vec![step.id.clone()],
        nodes: vec![GraphNode::Action(Box::new(ActionNode {
            step,
            bindings: BTreeMap::new(),
        }))],
        ..WorkflowGraph::default()
    };
    Task {
        id: format!("github-repositories-{ordinal}"),
        name: "Получить репозитории GitHub".into(),
        description: "Получить логин текущей учётной записи GitHub CLI и массив полной метаинформации о доступных репозиториях.".into(),
        platform: ppduster::rules::Platform::Macos,
        trust: TrustRequirement::ExternalAllowed,
        scenarios: Vec::new(),
        resolved_scenarios: Vec::new(),
        graph: Some(graph),
        steps: Vec::new(),
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
        .map(|steps| steps.into_iter().cloned().collect::<Vec<_>>())
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

    let mut nodes = Vec::with_capacity(generated_steps.len());
    let mut edges = Vec::with_capacity(generated_steps.len().saturating_sub(1));
    for (index, step) in generated_steps.into_iter().enumerate() {
        if let Some(previous) = nodes.last().map(GraphNode::id) {
            edges.push(GraphEdge::new(previous, EdgePort::Success, &step.id));
        }
        nodes.push(GraphNode::Action(Box::new(ActionNode {
            step,
            bindings: BTreeMap::new(),
        })));
        debug_assert_eq!(nodes.len(), index + 1);
    }
    task.steps.clear();
    task.graph = Some(WorkflowGraph {
        entries: nodes
            .first()
            .map(|node| vec![node.id().to_owned()])
            .unwrap_or_default(),
        nodes,
        edges,
        ..WorkflowGraph::default()
    });
    task.validate().map_err(anyhow::Error::msg)?;
    Ok(task)
}

fn github_picker_source_steps(task: &Task) -> Option<Vec<&Step>> {
    const PICKER_TEMPLATE_ID: &str = "github-repositories";
    if task.id != PICKER_TEMPLATE_ID
        || !matches!(task.trust, TrustRequirement::BundledOnly)
        || !task.steps.is_empty()
    {
        return None;
    }
    let graph = task.graph.as_ref()?;
    if graph.entries.len() != 1
        || graph.nodes.len() != 4
        || graph.edges.len() != 3
        || !graph.exits.is_empty()
        || !graph
            .nodes
            .iter()
            .all(|node| matches!(node, GraphNode::Action(_)))
    {
        return None;
    }

    let expected = [
        ActionKind::GitInspect,
        ActionKind::GitCloneIfMissing,
        ActionKind::GitFetch,
        ActionKind::GitFastForward,
    ];
    let mut current = graph.entries[0].as_str();
    let mut visited = BTreeSet::new();
    let mut steps = Vec::with_capacity(expected.len());
    for (index, expected_kind) in expected.into_iter().enumerate() {
        if !visited.insert(current.to_owned()) {
            return None;
        }
        let GraphNode::Action(action) = graph.nodes.iter().find(|node| node.id() == current)?
        else {
            return None;
        };
        if !action.bindings.is_empty()
            || !action.step.bindings.is_empty()
            || definition_for_action(&action.step.action).kind != expected_kind
        {
            return None;
        }
        steps.push(&action.step);
        let outgoing = graph
            .edges
            .iter()
            .filter(|edge| edge.from.node == current)
            .collect::<Vec<_>>();
        if index + 1 == expected.len() {
            if !outgoing.is_empty() {
                return None;
            }
        } else {
            let [edge] = outgoing.as_slice() else {
                return None;
            };
            if edge.from.port != EdgePort::Success {
                return None;
            }
            current = edge.to.node.as_str();
        }
    }
    (visited.len() == graph.nodes.len()).then_some(steps)
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

#[cfg(test)]
fn composer_block_id(kind: ComposerBlockKind) -> &'static str {
    match kind {
        ComposerBlockKind::GithubListRepositories => "list-github-repositories",
        ComposerBlockKind::ForEach => "for-each",
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

#[cfg(test)]
fn composer_step_context_lines(task: &Task, index: usize) -> Vec<String> {
    let Some(step) = task.steps.get(index) else {
        return Vec::new();
    };
    let definition = definition_for_action(&step.action);
    let mut lines = schema_context_lines(&definition.output_schema);
    let Action::ForEach {
        source_step,
        array_path,
        item,
        fields,
    } = &step.action
    else {
        return lines;
    };

    lines.clear();
    let Some(source) = composer_array_sources(task, index)
        .into_iter()
        .find(|source| source.step_id == *source_step && source.path == *array_path)
    else {
        return lines;
    };
    let item_type = project_item_type(&source.item_type, fields);
    match &item_type {
        ContextType::Object { schema } => {
            lines.push(format!("{item} : object (current item)"));
            collect_schema_context_lines(schema, item, &mut lines);
        }
        ContextType::Array { items } => lines.push(format!(
            "{item}[] : {} (current item)",
            context_type_label(items, false, false)
        )),
        _ => lines.push(format!(
            "{item} : {} (current item)",
            context_type_label(&item_type, false, false)
        )),
    }
    lines
}

fn schema_context_lines(schema: &ObjectSchema) -> Vec<String> {
    let mut lines = Vec::new();
    collect_schema_context_lines(schema, "", &mut lines);
    lines
}

fn collect_schema_context_lines(schema: &ObjectSchema, prefix: &str, lines: &mut Vec<String>) {
    for (name, field) in &schema.fields {
        let path = join_context_path(prefix, name);
        match &field.value_type {
            ContextType::Object { schema } => {
                lines.push(format!(
                    "{path} : {}",
                    context_type_label(&field.value_type, field.nullable, !field.required)
                ));
                collect_schema_context_lines(schema, &path, lines);
            }
            ContextType::Array { items } => {
                let array_path = format!("{path}[]");
                lines.push(format!(
                    "{array_path} : {}",
                    context_type_label(items, field.nullable, !field.required)
                ));
                if let ContextType::Object { schema } = items.as_ref() {
                    collect_schema_context_lines(schema, &array_path, lines);
                }
            }
            _ => lines.push(format!(
                "{path} : {}",
                context_type_label(&field.value_type, field.nullable, !field.required)
            )),
        }
    }
}

fn context_type_label(value_type: &ContextType, nullable: bool, optional: bool) -> String {
    let mut label = match value_type {
        ContextType::Any => "any".into(),
        ContextType::Null => "null".into(),
        ContextType::Boolean => "bool".into(),
        ContextType::Integer => "integer".into(),
        ContextType::Number => "number".into(),
        ContextType::String { format } => format
            .map(|format| format!("string<{}>", semantic_format_label(format)))
            .unwrap_or_else(|| "string".into()),
        ContextType::Array { items } => {
            format!("array<{}>", context_type_label(items, false, false))
        }
        ContextType::Object { .. } => "object".into(),
    };
    if nullable {
        label.push_str(" | null");
    }
    if optional {
        label.push_str(" (optional)");
    }
    label
}

fn semantic_format_label(format: SemanticFormat) -> &'static str {
    match format {
        SemanticFormat::Path => "path",
        SemanticFormat::FilePath => "file-path",
        SemanticFormat::DirectoryPath => "directory-path",
        SemanticFormat::Url => "url",
        SemanticFormat::GitUrl => "git-url",
        SemanticFormat::SecretRef => "secret-ref",
        SemanticFormat::Sha256 => "sha256",
        SemanticFormat::DateTime => "date-time",
        SemanticFormat::Duration => "duration",
        SemanticFormat::Email => "email",
        SemanticFormat::Hostname => "hostname",
        SemanticFormat::IpAddress => "ip-address",
        SemanticFormat::Uuid => "uuid",
        SemanticFormat::GitRef => "git-ref",
        SemanticFormat::RepositoryName => "repository-name",
        SemanticFormat::Identifier => "identifier",
    }
}

#[cfg(test)]
fn composer_step(kind: ComposerBlockKind, id: String) -> Step {
    let repository = "https://github.com/owner/repository.git".to_owned();
    let destination = "$HOME/Developer/owner/repository".to_owned();
    let action = match kind {
        ComposerBlockKind::GithubListRepositories => Action::GithubListRepositories,
        ComposerBlockKind::ForEach => Action::ForEach {
            source_step: "list-github-repositories-1".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        },
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
        name: block_definition(kind.action_kind()).title,
        bindings: BTreeMap::new(),
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
    task_action_steps(task)
        .into_iter()
        .map(|step| {
            describe_step(step, options)
                .unwrap_or_else(|error| format!("{}: не удалось описать шаг: {error:#}", step.id))
        })
        .collect()
}

fn task_action_steps(task: &Task) -> Vec<&Step> {
    fn visit<'a>(graph: &'a WorkflowGraph, output: &mut Vec<&'a Step>) {
        for node in &graph.nodes {
            match node {
                GraphNode::Action(node) => output.push(&node.step),
                GraphNode::ForEach(node) => visit(&node.body, output),
                GraphNode::If(node) => {
                    visit(&node.then_graph, output);
                    if let Some(graph) = &node.else_graph {
                        visit(graph, output);
                    }
                }
                GraphNode::Switch(node) => {
                    for case in &node.cases {
                        visit(&case.graph, output);
                    }
                    if let Some(graph) = &node.default {
                        visit(graph, output);
                    }
                }
                GraphNode::Join(_) => {}
            }
        }
    }

    if !task.steps.is_empty() {
        return task.steps.iter().collect();
    }
    let mut steps = Vec::new();
    if let Some(graph) = &task.graph {
        visit(graph, &mut steps);
    }
    steps
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
                step_count: task_action_steps(&resolved).len(),
                step_summaries: describe_task_steps(&resolved, options),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if !template.is_template() {
        return Ok(groups);
    }

    if let Some(configured) = configured {
        let base = pack.resolve(&template.id)?;
        let configured_steps = task_action_steps(configured);
        let base_steps = task_action_steps(&base);
        if configured_steps.len() != base_steps.len() {
            let source_step_id = base_steps
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
                .checked_add(configured_steps.len() - base_steps.len())
                .ok_or_else(|| anyhow::anyhow!("configured scenario group is too large"))?;
        }

        let mut offset = 0usize;
        for group in &mut groups {
            let end = offset
                .checked_add(group.step_count)
                .ok_or_else(|| anyhow::anyhow!("configured scenario group offset overflow"))?;
            let steps = configured_steps.get(offset..end).ok_or_else(|| {
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
        if offset != configured_steps.len() {
            anyhow::bail!(
                "configured task {} has {} ungrouped step(s)",
                configured.id,
                configured_steps.len() - offset
            );
        }
    }

    Ok(groups)
}

fn paint_bounded_code_lines(ui: &mut egui::Ui, source: &str, size: f32, color: Color32) {
    for line in source.split('\n') {
        ui.add(
            egui::Label::new(RichText::new(line).monospace().size(size).color(color)).truncate(),
        );
    }
}

fn paint_step_inspector(ui: &mut egui::Ui, step: &Step, options: Option<&RunOptions>, dark: bool) {
    ui.add(
        egui::Label::new(
            RichText::new(step_title(step))
                .strong()
                .size(14.0)
                .color(text(dark)),
        )
        .truncate(),
    );
    ui.add(egui::Label::new(RichText::new(&step.id).monospace().size(9.0).color(MUTED)).truncate());
    if let Some(options) = options {
        let summary = describe_step(step, options)
            .unwrap_or_else(|error| format!("Не удалось описать шаг: {error:#}"));
        ui.add_space(8.0);
        ui.add(egui::Label::new(RichText::new(summary).size(9.0).color(PURPLE)).truncate());
    }
    ui.add_space(8.0);
    let yaml = serde_yaml::to_string(step).unwrap_or_else(|error| format!("Ошибка: {error}"));
    Frame::new()
        .fill(code_surface(dark))
        .corner_radius(8)
        .inner_margin(Margin::same(9))
        .show(ui, |ui| {
            paint_bounded_code_lines(ui, &yaml, 9.0, text(dark));
        });
}

fn paint_composer_conditions(
    ui: &mut egui::Ui,
    step: &mut Step,
    fields: &[ComposerConditionField],
    dark: bool,
) -> bool {
    let mut changed = false;
    let step_id = step.id.clone();
    section_label(ui, "УСЛОВИЯ");
    ui.label(
        RichText::new("Доступны только типизированные поля предыдущих блоков.")
            .size(8.0)
            .color(MUTED),
    );
    ui.add_space(5.0);
    changed |= paint_condition_slot(
        ui,
        &step_id,
        "when",
        "Выполнять, когда",
        &mut step.when,
        fields,
        dark,
    );
    ui.add_space(8.0);
    changed |= paint_condition_slot(
        ui,
        &step_id,
        "require",
        "Требовать перед запуском",
        &mut step.require,
        fields,
        dark,
    );
    changed
}

fn paint_condition_slot(
    ui: &mut egui::Ui,
    step_id: &str,
    slot_id: &str,
    title: &str,
    condition: &mut Option<StepCondition>,
    fields: &[ComposerConditionField],
    dark: bool,
) -> bool {
    let mut changed = false;
    let mut enabled = condition.is_some();
    let toggle = ui.add_enabled(
        enabled || !fields.is_empty(),
        egui::Checkbox::new(&mut enabled, title),
    );
    if toggle.changed() {
        if enabled {
            if let Some(field) = default_condition_field(fields) {
                let rule = default_simple_condition(field);
                *condition = Some(StepCondition::Expression {
                    rule: build_simple_condition_rule(&rule),
                    policy: RuleOutcomePolicy::default(),
                });
            }
        } else {
            *condition = None;
        }
        changed = true;
    }
    if !enabled {
        if fields.is_empty() {
            ui.label(
                RichText::new("Нет предыдущего блока с выходным контекстом.")
                    .size(8.0)
                    .color(MUTED),
            );
        }
        return changed;
    }

    let Some(condition_value) = condition.as_mut() else {
        return changed;
    };
    let condition_yaml = serde_yaml::to_string(&*condition_value)
        .unwrap_or_else(|error| format!("Не удалось показать условие: {error}"));
    // Replacing an unsupported typed AST is an explicit model edit, but its
    // null/missing/unknown policy is independent of the AST shape and must not
    // silently reset. Legacy conditions have no such policy.
    let replacement_policy = match &*condition_value {
        StepCondition::Expression { policy, .. } => *policy,
        _ => RuleOutcomePolicy::default(),
    };
    let mut replace_with_simple = false;
    match condition_value {
        StepCondition::Expression { rule, policy } => {
            if let Some(mut editable) = composer_condition_rule(rule)
                .filter(|editable| composer_condition_rule_supported(editable, fields))
            {
                let editor_changed = paint_composer_condition_rule_editor(
                    ui,
                    &format!("{step_id}-{slot_id}"),
                    &mut editable,
                    fields,
                    dark,
                );
                if editor_changed {
                    *rule = build_composer_condition_rule(&editable);
                    changed = true;
                }
                changed |= paint_rule_outcome_policy(ui, &format!("{step_id}-{slot_id}"), policy);
            } else {
                ui.label(
                    RichText::new(
                        "Расширенное typed-выражение сохранено без изменений (read-only).",
                    )
                    .size(8.0)
                    .color(ORANGE),
                );
                paint_condition_yaml(ui, &condition_yaml, dark);
                changed |= paint_rule_outcome_policy(ui, &format!("{step_id}-{slot_id}"), policy);
                replace_with_simple = ui
                    .add_enabled(
                        !fields.is_empty(),
                        egui::Button::new("Заменить простым typed-условием"),
                    )
                    .clicked();
            }
        }
        StepCondition::ExitCode { .. }
        | StepCondition::Path { .. }
        | StepCondition::All { .. }
        | StepCondition::Any { .. }
        | StepCondition::Not { .. } => {
            ui.label(
                RichText::new("Legacy-условие сохранено без изменений (read-only).")
                    .size(8.0)
                    .color(ORANGE),
            );
            paint_condition_yaml(ui, &condition_yaml, dark);
            replace_with_simple = ui
                .add_enabled(
                    !fields.is_empty(),
                    egui::Button::new("Заменить typed-условием"),
                )
                .clicked();
        }
    }
    if replace_with_simple {
        if let Some(field) = default_condition_field(fields) {
            let rule = default_simple_condition(field);
            *condition_value = StepCondition::Expression {
                rule: build_simple_condition_rule(&rule),
                policy: replacement_policy,
            };
            changed = true;
        }
    }
    changed
}

fn paint_composer_condition_rule_editor(
    ui: &mut egui::Ui,
    editor_id: &str,
    rule: &mut ComposerConditionRule,
    fields: &[ComposerConditionField],
    dark: bool,
) -> bool {
    if !composer_condition_rule_fits_editor(rule) {
        ui.label(
            RichText::new("Условие превышает лимиты визуального редактора и сохранено read-only.")
                .size(8.0)
                .color(ORANGE),
        );
        return false;
    }
    let mut total_nodes = composer_condition_rule_nodes(rule);
    paint_composer_condition_rule_editor_inner(
        ui,
        editor_id,
        rule,
        fields,
        dark,
        0,
        &mut total_nodes,
    )
}

fn paint_composer_condition_rule_editor_inner(
    ui: &mut egui::Ui,
    editor_id: &str,
    rule: &mut ComposerConditionRule,
    fields: &[ComposerConditionField],
    dark: bool,
    depth: usize,
    total_nodes: &mut usize,
) -> bool {
    let mut changed = false;
    let mut replacement = None;
    let group_is_all = match rule {
        ComposerConditionRule::All(_) => Some(true),
        ComposerConditionRule::Any(_) => Some(false),
        ComposerConditionRule::Clause(_) | ComposerConditionRule::Not(_) => None,
    };
    match rule {
        ComposerConditionRule::Clause(clause) => {
            changed |= paint_simple_condition_editor(ui, editor_id, clause, fields, dark);
        }
        ComposerConditionRule::All(rules) | ComposerConditionRule::Any(rules) => {
            let is_all = group_is_all.expect("group kind is known");
            ui.label(
                RichText::new(if is_all {
                    "Все условия (И)"
                } else {
                    "Хотя бы одно условие (ИЛИ)"
                })
                .strong()
                .size(9.0)
                .color(PURPLE),
            );
            let can_remove = rules.len() > 1;
            let mut remove = None;
            for (index, child) in rules.iter_mut().enumerate() {
                ui.push_id((editor_id, index), |ui| {
                    Frame::new()
                        .fill(code_surface(dark))
                        .corner_radius(7)
                        .inner_margin(Margin::same(7))
                        .show(ui, |ui| {
                            changed |= paint_composer_condition_rule_editor_inner(
                                ui,
                                &format!("{editor_id}-{index}"),
                                child,
                                fields,
                                dark,
                                depth + 1,
                                total_nodes,
                            );
                            if can_remove && ui.small_button("Удалить условие").clicked()
                            {
                                remove = Some(index);
                            }
                        });
                });
            }
            if let Some(index) = remove {
                *total_nodes =
                    (*total_nodes).saturating_sub(composer_condition_rule_nodes(&rules[index]));
                rules.remove(index);
                changed = true;
            }
            let can_add = *total_nodes < CONDITION_EDITOR_MAX_NODES
                && depth.saturating_add(1) <= CONDITION_EDITOR_MAX_DEPTH;
            if ui
                .add_enabled(can_add, egui::Button::new("+ Добавить условие").small())
                .clicked()
            {
                if let Some(field) = default_condition_field(fields) {
                    rules.push(ComposerConditionRule::Clause(default_simple_condition(
                        field,
                    )));
                    *total_nodes += 1;
                    changed = true;
                }
            }
            if ui
                .small_button(if is_all {
                    "Сменить на ИЛИ"
                } else {
                    "Сменить на И"
                })
                .clicked()
            {
                replacement = Some(if is_all {
                    ComposerConditionRule::Any(rules.clone())
                } else {
                    ComposerConditionRule::All(rules.clone())
                });
            }
        }
        ComposerConditionRule::Not(child) => {
            ui.label(RichText::new("НЕ").strong().size(9.0).color(PURPLE));
            ui.indent((editor_id, "not"), |ui| {
                changed |= paint_composer_condition_rule_editor_inner(
                    ui,
                    &format!("{editor_id}-not"),
                    child,
                    fields,
                    dark,
                    depth + 1,
                    total_nodes,
                );
            });
            if ui.small_button("Убрать НЕ").clicked() {
                replacement = Some((**child).clone());
            }
        }
    }

    if replacement.is_none() {
        if let Some(field) = default_condition_field(fields) {
            let default = ComposerConditionRule::Clause(default_simple_condition(field));
            ui.horizontal_wrapped(|ui| {
                let all = ComposerConditionRule::All(vec![rule.clone(), default.clone()]);
                if ui
                    .add_enabled(
                        composer_condition_replacement_fits(rule, &all, depth, *total_nodes),
                        egui::Button::new("Обернуть в И").small(),
                    )
                    .clicked()
                {
                    replacement = Some(all);
                }
                let any = ComposerConditionRule::Any(vec![rule.clone(), default]);
                if ui
                    .add_enabled(
                        composer_condition_replacement_fits(rule, &any, depth, *total_nodes),
                        egui::Button::new("Обернуть в ИЛИ").small(),
                    )
                    .clicked()
                {
                    replacement = Some(any);
                }
                let not = ComposerConditionRule::Not(Box::new(rule.clone()));
                if ui
                    .add_enabled(
                        composer_condition_replacement_fits(rule, &not, depth, *total_nodes),
                        egui::Button::new("Обернуть в НЕ").small(),
                    )
                    .clicked()
                {
                    replacement = Some(not);
                }
            });
        }
    }
    if let Some(replacement) = replacement {
        *total_nodes = (*total_nodes)
            .saturating_sub(composer_condition_rule_nodes(rule))
            .saturating_add(composer_condition_rule_nodes(&replacement));
        *rule = replacement;
        changed = true;
    }
    changed
}

fn paint_simple_condition_editor(
    ui: &mut egui::Ui,
    editor_id: &str,
    rule: &mut SimpleConditionRule,
    fields: &[ComposerConditionField],
    dark: bool,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new("Поле контекста").size(8.0).color(MUTED));
    let selected_label = fields
        .iter()
        .find(|field| field.reference == rule.field)
        .map(|field| field.label.clone())
        .unwrap_or_else(|| format!("Недоступно: {}", field_ref_label(&rule.field)));
    egui::ComboBox::from_id_salt((editor_id, "field"))
        .selected_text(selected_label)
        .truncate()
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for field in fields {
                if ui
                    .selectable_label(field.reference == rule.field, &field.label)
                    .clicked()
                {
                    rule.field = field.reference.clone();
                    let operators = condition_operators(&field.value_type);
                    if !operators.contains(&rule.operator) {
                        rule.operator = operators
                            .first()
                            .copied()
                            .unwrap_or(ComposerConditionOperator::Exists);
                    }
                    rule.literal = default_condition_literal(field, rule.operator);
                    changed = true;
                    ui.close();
                }
            }
        });

    let Some(field) = fields.iter().find(|field| field.reference == rule.field) else {
        ui.label(
            RichText::new("Ссылка больше не видима на этой позиции. Выберите предыдущий блок.")
                .size(8.0)
                .color(ORANGE),
        );
        return changed;
    };
    ui.label(
        RichText::new(if field.required {
            "Поле гарантировано схемой"
        } else {
            "Поле может отсутствовать"
        })
        .size(8.0)
        .color(MUTED),
    );
    ui.label(RichText::new("Операция").size(8.0).color(MUTED));
    let operators = condition_operators(&field.value_type);
    if !operators.contains(&rule.operator) {
        rule.operator = operators
            .first()
            .copied()
            .unwrap_or(ComposerConditionOperator::Exists);
        rule.literal = default_condition_literal(field, rule.operator);
        changed = true;
    }
    egui::ComboBox::from_id_salt((editor_id, "operator"))
        .selected_text(rule.operator.label())
        .truncate()
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for operator in &operators {
                if ui
                    .selectable_label(*operator == rule.operator, operator.label())
                    .clicked()
                {
                    rule.operator = *operator;
                    rule.literal = default_condition_literal(field, *operator);
                    changed = true;
                    ui.close();
                }
            }
        });

    if rule.operator.requires_literal() {
        changed |= paint_condition_literal(ui, editor_id, field, rule);
    } else {
        rule.literal = None;
    }
    if field.nullable {
        ui.label(
            RichText::new("Поле допускает null — поведение задаётся политикой ниже.")
                .size(8.0)
                .color(PURPLE),
        );
    }
    Frame::new()
        .fill(code_surface(dark))
        .corner_radius(7)
        .inner_margin(Margin::same(7))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "{} {}",
                    field_ref_label(&rule.field),
                    rule.operator.label()
                ))
                .monospace()
                .size(8.0)
                .color(PURPLE),
            );
        });
    changed
}

fn paint_condition_literal(
    ui: &mut egui::Ui,
    editor_id: &str,
    field: &ComposerConditionField,
    rule: &mut SimpleConditionRule,
) -> bool {
    let mut changed = false;
    let kinds = condition_literal_kinds(field, rule.operator);
    if kinds.is_empty() {
        rule.literal = None;
        return changed;
    }
    let current_kind = rule
        .literal
        .as_ref()
        .and_then(ComposerLiteralKind::from_value);
    if current_kind.is_none_or(|kind| !kinds.contains(&kind)) {
        rule.literal = Some(kinds[0].default_value());
        changed = true;
    }
    let mut selected_kind = rule
        .literal
        .as_ref()
        .and_then(ComposerLiteralKind::from_value)
        .unwrap_or(kinds[0]);
    if kinds.len() > 1 {
        ui.label(RichText::new("Тип значения").size(8.0).color(MUTED));
        egui::ComboBox::from_id_salt((editor_id, "literal-kind"))
            .selected_text(selected_kind.label())
            .truncate()
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for kind in &kinds {
                    if ui
                        .selectable_label(*kind == selected_kind, kind.label())
                        .clicked()
                    {
                        selected_kind = *kind;
                        rule.literal = Some(kind.default_value());
                        changed = true;
                        ui.close();
                    }
                }
            });
    }
    ui.label(RichText::new("Значение").size(8.0).color(MUTED));
    if let Some(literal) = rule.literal.as_mut() {
        changed |= match literal {
            ExpressionValue::Null => {
                ui.label(RichText::new("null").monospace().size(9.0).color(PURPLE));
                false
            }
            ExpressionValue::Bool(value) => ui.checkbox(value, "true").changed(),
            ExpressionValue::Int(value) => ui.add(egui::DragValue::new(value)).changed(),
            ExpressionValue::UInt(value) => ui.add(egui::DragValue::new(value)).changed(),
            ExpressionValue::Float(value) => {
                ui.add(egui::DragValue::new(value).speed(0.1)).changed()
            }
            ExpressionValue::String(value) => ui.text_edit_singleline(value).changed(),
            ExpressionValue::List(_) | ExpressionValue::Object(_) => false,
        };
        if rule.operator == ComposerConditionOperator::Matches {
            if let ExpressionValue::String(pattern) = literal {
                match regex_pattern_error(pattern) {
                    Some(error) => {
                        ui.label(RichText::new(error).size(8.0).color(ORANGE));
                    }
                    None => {
                        ui.label(
                            RichText::new("Регулярное выражение корректно")
                                .size(8.0)
                                .color(CYAN),
                        );
                    }
                }
            }
        }
    }
    changed
}

fn paint_rule_outcome_policy(
    ui: &mut egui::Ui,
    editor_id: &str,
    policy: &mut RuleOutcomePolicy,
) -> bool {
    ui.add_space(5.0);
    ui.label(
        RichText::new("Явная политика неопределённого результата")
            .size(8.0)
            .color(MUTED),
    );
    let mut changed = false;
    changed |=
        paint_indeterminate_policy(ui, editor_id, "on-null", "Если null", &mut policy.on_null);
    changed |= paint_indeterminate_policy(
        ui,
        editor_id,
        "on-missing",
        "Если отсутствует",
        &mut policy.on_missing,
    );
    changed |= paint_indeterminate_policy(
        ui,
        editor_id,
        "on-unknown",
        "Если неизвестно",
        &mut policy.on_unknown,
    );
    changed
}

fn paint_indeterminate_policy(
    ui: &mut egui::Ui,
    editor_id: &str,
    policy_id: &str,
    label: &str,
    value: &mut IndeterminatePolicy,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(8.0).color(MUTED));
        egui::ComboBox::from_id_salt((editor_id, policy_id))
            .selected_text(indeterminate_policy_label(*value))
            .truncate()
            .show_ui(ui, |ui| {
                for policy in [
                    IndeterminatePolicy::Fail,
                    IndeterminatePolicy::TreatAsFalse,
                    IndeterminatePolicy::TreatAsTrue,
                ] {
                    if ui
                        .selectable_label(policy == *value, indeterminate_policy_label(policy))
                        .clicked()
                    {
                        *value = policy;
                        changed = true;
                        ui.close();
                    }
                }
            });
    });
    changed
}

const fn indeterminate_policy_label(policy: IndeterminatePolicy) -> &'static str {
    match policy {
        IndeterminatePolicy::Fail => "Ошибка",
        IndeterminatePolicy::TreatAsFalse => "Считать false",
        IndeterminatePolicy::TreatAsTrue => "Считать true",
    }
}

fn paint_condition_yaml(ui: &mut egui::Ui, yaml: &str, dark: bool) {
    Frame::new()
        .fill(code_surface(dark))
        .corner_radius(7)
        .inner_margin(Margin::same(7))
        .show(ui, |ui| {
            paint_bounded_code_lines(ui, yaml, 8.0, text(dark));
        });
}

fn field_ref_label(reference: &FieldRef) -> String {
    let mut label = match &reference.scope {
        ContextScope::Scenario => "scenario".into(),
        ContextScope::Step { step_id } => step_id.clone(),
        ContextScope::LoopItem { step_id } => format!("loop:{step_id}"),
    };
    for segment in &reference.segments {
        match segment {
            ContextPathSegment::Field { name } => {
                label.push('.');
                label.push_str(name);
            }
            ContextPathSegment::Index { index } => label.push_str(&format!("[{index}]")),
        }
    }
    label
}

#[cfg(test)]
#[allow(dead_code)]
fn paint_composer_step_editor(
    ui: &mut egui::Ui,
    step: &mut Step,
    array_sources: &[ComposerArraySource],
    loop_sources: &[ComposerLoopSource],
    parent_loop_source: Option<&ComposerLoopSource>,
    dark: bool,
) -> bool {
    let mut changed = false;
    let is_git_fetch = matches!(&step.action, Action::GitFetch { .. });
    let input_schema = definition_for_action(&step.action).input_schema;
    ui.label(RichText::new("Название блока").size(9.0).color(MUTED));
    changed |= ui.text_edit_singleline(&mut step.name).changed();
    ui.label(RichText::new("ID блока").size(9.0).color(MUTED));
    changed |= ui.text_edit_singleline(&mut step.id).changed();
    let editor_step_id = step.id.clone();
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
        Action::ForEach {
            source_step,
            array_path,
            item,
            fields,
        } => {
            ui.label(RichText::new("Массив для перебора").size(9.0).color(MUTED));
            let selected_source = array_sources.iter().find(|source| {
                source.step_id == *source_step && source.path == array_path.as_str()
            });
            let selected_label = selected_source
                .map(|source| format!("{}[]", source.path))
                .unwrap_or_else(|| "Массив не выбран".into());
            egui::ComboBox::from_id_salt(("foreach-array-source", step.id.clone()))
                .selected_text(selected_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for source in array_sources {
                        let selected =
                            source.step_id == *source_step && source.path == array_path.as_str();
                        if ui
                            .selectable_label(
                                selected,
                                format!("{}[] → {}", source.path, truncate(&source.step_name, 20)),
                            )
                            .clicked()
                        {
                            *source_step = source.step_id.clone();
                            *array_path = source.path.clone();
                            *item = source.item.clone();
                            *fields = item_object_fields(&source.item_type)
                                .into_iter()
                                .map(|(name, _)| name)
                                .collect();
                            changed = true;
                            ui.close();
                        }
                    }
                });
            if array_sources.is_empty() {
                ui.label(
                    RichText::new("Перед циклом нет блока с массивом в выходном контексте.")
                        .size(8.0)
                        .color(ORANGE),
                );
            } else {
                ui.label(
                    RichText::new(format!("Текущий элемент: {item}"))
                        .monospace()
                        .size(8.0)
                        .color(PURPLE),
                );
            }
            ui.add_space(7.0);
            ui.label(
                RichText::new("Поля для следующего блока")
                    .size(9.0)
                    .color(MUTED),
            );
            let available_fields = selected_source
                .map(|source| item_object_fields(&source.item_type))
                .unwrap_or_default();
            if available_fields.is_empty() {
                ui.label(
                    RichText::new(
                        "Элемент массива скалярный или не имеет известной объектной схемы.",
                    )
                    .size(8.0)
                    .color(MUTED),
                );
            } else {
                ui.horizontal(|ui| {
                    if ui.small_button("Все").clicked() {
                        *fields = available_fields
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect();
                        changed = true;
                    }
                    let clone_fields = clone_item_field_names(&available_fields);
                    if ui
                        .add_enabled(
                            !clone_fields.is_empty(),
                            egui::Button::new("Для клонирования"),
                        )
                        .clicked()
                    {
                        *fields = clone_fields;
                        changed = true;
                    }
                });
                Frame::new()
                    .fill(code_surface(dark))
                    .corner_radius(8)
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        let inherited_all = fields.is_empty();
                        for (field, schema) in &available_fields {
                            let mut selected =
                                inherited_all || fields.iter().any(|value| value == field);
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut selected, "").changed() {
                                    if inherited_all {
                                        *fields = available_fields
                                            .iter()
                                            .map(|(name, _)| name.clone())
                                            .collect();
                                    }
                                    if selected {
                                        if !fields.iter().any(|value| value == field) {
                                            fields.push(field.clone());
                                        }
                                    } else {
                                        fields.retain(|value| value != field);
                                    }
                                    changed = true;
                                }
                                ui.label(RichText::new(field).monospace().size(9.0).color(PURPLE));
                                ui.label(
                                    RichText::new(context_type_label(
                                        &schema.value_type,
                                        schema.nullable,
                                        !schema.required,
                                    ))
                                    .monospace()
                                    .size(8.0)
                                    .color(MUTED),
                                );
                            });
                        }
                    });
            }
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "Контекст итерации — только дочернему блоку: {{{{{item}.field}}}} (например, {{{{{item}.https_url}}}})."
                    ))
                    .size(9.0)
                    .color(PURPLE),
                )
                .wrap(),
            );
        }
        Action::ForEachGitCloneIfMissing {
            loop_step,
            repo,
            dest,
            branch,
        } => {
            ui.label(RichText::new("Цикл-источник").size(9.0).color(MUTED));
            let selected_loop = loop_sources
                .iter()
                .find(|source| source.step_id == *loop_step);
            let loop_label = selected_loop
                .map(|source| source.step_name.clone())
                .unwrap_or_else(|| "Выберите предыдущий For each".into());
            egui::ComboBox::from_id_salt(("clone-loop-source", editor_step_id.clone()))
                .selected_text(loop_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for source in loop_sources {
                        if ui
                            .selectable_label(
                                source.step_id == *loop_step,
                                format!("{} → {}", source.step_name, source.item),
                            )
                            .clicked()
                        {
                            *loop_step = source.step_id.clone();
                            if let Some((_, template)) =
                                input_schema.field("repo").and_then(|field| {
                                    composer_context_options(source, &field.value_type)
                                        .into_iter()
                                        .next()
                                })
                            {
                                *repo = template;
                            }
                            if let Some((_, template)) =
                                input_schema.field("dest").and_then(|field| {
                                    composer_destination_options(source, &field.value_type)
                                        .into_iter()
                                        .next()
                                })
                            {
                                *dest = template;
                            }
                            *branch = input_schema.field("branch").and_then(|field| {
                                composer_context_options(source, &field.value_type)
                                    .into_iter()
                                    .next()
                                    .map(|(_, template)| template)
                            });
                            changed = true;
                            ui.close();
                        }
                    }
                });

            let selected_loop = loop_sources
                .iter()
                .find(|source| source.step_id == *loop_step);
            if let Some(source) = selected_loop {
                let repository_options = input_schema
                    .field("repo")
                    .map(|field| composer_context_options(source, &field.value_type))
                    .unwrap_or_default();
                changed |= composer_binding_selector(
                    ui,
                    &editor_step_id,
                    "Repository URL",
                    repo,
                    &repository_options,
                );

                let destination_options = input_schema
                    .field("dest")
                    .map(|field| composer_destination_options(source, &field.value_type))
                    .unwrap_or_default();
                changed |= composer_binding_selector(
                    ui,
                    &editor_step_id,
                    "Локальная папка",
                    dest,
                    &destination_options,
                );

                let branch_options = input_schema
                    .field("branch")
                    .map(|field| composer_context_options(source, &field.value_type))
                    .unwrap_or_default();
                if let Some((_, default_template)) = branch_options.first() {
                    let branch_value = branch.get_or_insert_with(|| default_template.clone());
                    changed |= composer_binding_selector(
                        ui,
                        &editor_step_id,
                        "Ветка",
                        branch_value,
                        &branch_options,
                    );
                } else {
                    ui.label(RichText::new("Ветка").size(9.0).color(MUTED));
                    ui.label(
                        RichText::new("В контексте нет поля формата git-ref")
                            .size(8.0)
                            .color(ORANGE),
                    );
                }
            } else {
                ui.label(
                    RichText::new("Перед клонированием нет блока For each.")
                        .size(8.0)
                        .color(ORANGE),
                );
            }
            changed |= composer_git_auth(ui, &mut step.auth);
        }
        Action::GitInspect { repo, dest } => {
            section_label(ui, "ВХОДНОЙ КОНТЕКСТ");
            changed |= composer_input_editor(
                ui,
                &editor_step_id,
                "repo",
                "Repository URL",
                repo,
                input_schema.field("repo"),
                array_sources,
                parent_loop_source,
                &mut step.bindings,
                dark,
            );
            ui.add_space(7.0);
            changed |= composer_input_editor(
                ui,
                &editor_step_id,
                "dest",
                "Локальная папка",
                dest,
                input_schema.field("dest"),
                array_sources,
                parent_loop_source,
                &mut step.bindings,
                dark,
            );
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

#[cfg(test)]
#[allow(dead_code)]
fn composer_text_field(ui: &mut egui::Ui, label: &str, value: &mut String) -> bool {
    ui.label(RichText::new(label).size(9.0).color(MUTED));
    ui.text_edit_singleline(value).changed()
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(dead_code)]
fn composer_input_editor(
    ui: &mut egui::Ui,
    step_id: &str,
    target: &str,
    label: &str,
    manual_value: &mut String,
    expected: Option<&FieldSchema>,
    array_sources: &[ComposerArraySource],
    parent_loop_source: Option<&ComposerLoopSource>,
    bindings: &mut BTreeMap<String, Binding>,
    dark: bool,
) -> bool {
    if let Some(loop_source) = parent_loop_source {
        return composer_loop_input_editor(
            ui,
            step_id,
            target,
            label,
            manual_value,
            expected,
            loop_source,
            array_sources,
            bindings,
            dark,
        );
    }
    composer_indexed_input_editor(
        ui,
        step_id,
        target,
        label,
        manual_value,
        expected,
        array_sources,
        bindings,
        dark,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(dead_code)]
fn composer_loop_input_editor(
    ui: &mut egui::Ui,
    step_id: &str,
    target: &str,
    label: &str,
    manual_value: &mut String,
    expected: Option<&FieldSchema>,
    loop_source: &ComposerLoopSource,
    array_sources: &[ComposerArraySource],
    bindings: &mut BTreeMap<String, Binding>,
    dark: bool,
) -> bool {
    ui.label(RichText::new(label).size(9.0).color(MUTED));
    let Some(expected) = expected else {
        ui.label(
            RichText::new("У блока нет типизированного контракта для этого входа.")
                .size(8.0)
                .color(ORANGE),
        );
        return false;
    };

    if target == "dest" {
        return composer_loop_destination_input_editor(
            ui,
            step_id,
            target,
            manual_value,
            loop_source,
            bindings,
            dark,
        );
    }

    let fields = composer_loop_field_options(loop_source, expected);
    let membership_required = target == "repo";
    let existing = bindings.get(target).cloned();
    let loop_sources = std::slice::from_ref(loop_source);
    let parsed_loop = existing
        .as_ref()
        .and_then(|binding| composer_loop_binding_selection(binding, loop_sources));
    // Projects created before loop-item bindings existed may contain an array
    // index here. Detect it only when the canvas proves this consumer is the
    // immediate child and the indexed array is exactly the loop collection,
    // but require an explicit click because the semantic rewrite is material.
    let indexed_migration = existing
        .as_ref()
        .and_then(|binding| composer_indexed_binding_for_loop(binding, loop_source, array_sources));
    let mut selection = parsed_loop;
    let mut supported = selection.as_ref().is_some_and(|selection| {
        selection.loop_step == loop_source.step_id
            && fields
                .iter()
                .any(|field| field.path == selection.field_path)
    });
    let mut changed = false;
    if membership_required && existing.is_none() {
        let Some(field) = fields.first() else {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Текущий элемент цикла не содержит обязательного поля Repository URL.",
                    )
                    .size(8.0)
                    .color(ORANGE),
                )
                .wrap(),
            );
            return false;
        };
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Repository URL ещё не привязан. Для дочернего блока For each нужен текущий элемент цикла.",
                )
                .size(8.0)
                .color(ORANGE),
            )
            .wrap(),
        );
        if ui.button("Использовать текущий элемент цикла").clicked()
        {
            changed |=
                composer_insert_default_loop_binding(bindings, target, loop_source, expected);
            selection = Some(ComposerLoopBinding {
                loop_step: loop_source.step_id.clone(),
                field_path: field.path.clone(),
            });
            supported = true;
        } else {
            return false;
        }
    }
    if let Some(migration) = indexed_migration.filter(|migration| {
        fields
            .iter()
            .any(|field| field.path == migration.field_path)
    }) {
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Сохранена привязка к номеру элемента массива. Дочерний блок For each должен получать текущий элемент каждой итерации.",
                )
                .size(8.0)
                .color(ORANGE),
            )
            .wrap(),
        );
        if ui.button("Использовать текущий элемент цикла").clicked()
        {
            bindings.insert(
                target.into(),
                composer_loop_binding(loop_source, &migration.field_path),
            );
            selection = Some(migration);
            supported = true;
            changed = true;
        } else {
            return false;
        }
    }

    if let Some(binding) = existing.as_ref().filter(|_| !supported) {
        ui.label(
            RichText::new("Привязка задана в YAML и не поддерживается этим визуальным редактором.")
                .size(8.0)
                .color(ORANGE),
        );
        let binding_text = match binding {
            Binding::Field { field } => field_ref_label(field),
            _ => serde_yaml::to_string(binding)
                .unwrap_or_else(|_| "не удалось показать привязку".into())
                .trim()
                .to_owned(),
        };
        Frame::new()
            .fill(code_surface(dark))
            .corner_radius(7)
            .inner_margin(Margin::same(7))
            .show(ui, |ui| {
                paint_bounded_code_lines(ui, &binding_text, 8.0, text(dark));
            });
        if membership_required {
            let Some(field) = fields.first() else {
                return changed;
            };
            if ui.button("Использовать текущий элемент цикла").clicked()
            {
                selection = Some(ComposerLoopBinding {
                    loop_step: loop_source.step_id.clone(),
                    field_path: field.path.clone(),
                });
                bindings.insert(
                    target.into(),
                    composer_loop_binding(loop_source, &field.path),
                );
                supported = true;
                changed = true;
            } else {
                return changed;
            }
        } else {
            if ui.small_button("Заменить ручным вводом").clicked() {
                bindings.remove(target);
                return true;
            }
            return changed;
        }
    }

    let mut use_context = supported || membership_required;
    let was_context = use_context;
    ui.label(RichText::new("Источник значения").size(8.0).color(MUTED));
    if membership_required {
        Frame::new()
            .fill(code_surface(dark))
            .corner_radius(7)
            .inner_margin(Margin::same(7))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Текущий элемент цикла (обязательно)")
                        .size(8.0)
                        .color(PURPLE),
                );
            });
    } else {
        egui::ComboBox::from_id_salt(("loop-input-mode", step_id, target))
            .selected_text(if use_context {
                "Текущий элемент цикла"
            } else {
                "Вручную"
            })
            .width(ui.available_width())
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut use_context, false, "Вручную");
                ui.add_enabled_ui(!fields.is_empty(), |ui| {
                    ui.selectable_value(&mut use_context, true, "Текущий элемент цикла");
                });
            });
    }

    if use_context != was_context {
        if use_context {
            if let Some(field) = fields.first() {
                selection = Some(ComposerLoopBinding {
                    loop_step: loop_source.step_id.clone(),
                    field_path: field.path.clone(),
                });
                bindings.insert(
                    target.into(),
                    composer_loop_binding(loop_source, &field.path),
                );
                changed = true;
            }
        } else {
            selection = None;
            bindings.remove(target);
            changed = true;
        }
    }

    if !use_context {
        changed |= ui.text_edit_singleline(manual_value).changed();
        if fields.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new("В текущем элементе цикла нет поля совместимого типа.")
                        .size(8.0)
                        .color(MUTED),
                )
                .wrap(),
            );
        }
        return changed;
    }

    let Some(mut selection) = selection else {
        ui.label(
            RichText::new("Не удалось прочитать привязку к текущему элементу цикла.")
                .size(8.0)
                .color(ORANGE),
        );
        return changed;
    };

    ui.label(
        RichText::new("Текущий элемент цикла")
            .size(8.0)
            .color(MUTED),
    );
    let loop_label = format!(
        "{} → {}",
        truncate(&loop_source.step_name, 28),
        loop_source.item
    );
    Frame::new()
        .fill(code_surface(dark))
        .corner_radius(7)
        .inner_margin(Margin::same(7))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(&loop_label)
                        .monospace()
                        .size(8.0)
                        .color(PURPLE),
                )
                .truncate(),
            )
            .on_hover_text(format!(
                "{} ({}) → {}",
                loop_source.step_name, loop_source.step_id, loop_source.item
            ));
        });

    ui.label(
        RichText::new("Поле текущего элемента")
            .size(8.0)
            .color(MUTED),
    );
    let field_label = fields
        .iter()
        .find(|field| field.path == selection.field_path)
        .map(|field| {
            let name = if field.path.is_empty() {
                "Элемент целиком"
            } else {
                &field.path
            };
            format!(
                "{} · {}",
                name,
                context_type_label(&field.value_type, field.nullable, !field.required)
            )
        })
        .unwrap_or_else(|| "Поле не выбрано".into());
    let field_response = egui::ComboBox::from_id_salt(("loop-input-field", step_id, target))
        .selected_text(field_label.clone())
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            for field in &fields {
                let name = if field.path.is_empty() {
                    "Элемент целиком"
                } else {
                    &field.path
                };
                let label = format!(
                    "{} · {}",
                    name,
                    context_type_label(&field.value_type, field.nullable, !field.required)
                );
                if ui
                    .add(
                        egui::Button::selectable(field.path == selection.field_path, label)
                            .truncate()
                            .min_size(Vec2::new(ui.available_width(), 0.0)),
                    )
                    .clicked()
                {
                    selection.field_path = field.path.clone();
                    changed = true;
                    ui.close();
                }
            }
        });
    field_response.response.on_hover_text(field_label);

    if changed {
        bindings.insert(
            target.into(),
            composer_loop_binding(loop_source, &selection.field_path),
        );
    }
    let preview = composer_loop_binding_preview(loop_source, &selection, target);
    ui.add(
        egui::Label::new(RichText::new(&preview).monospace().size(8.0).color(PURPLE)).truncate(),
    )
    .on_hover_text(format!(
        "{} → {target}",
        field_ref_label(&composer_loop_field_ref(loop_source, &selection.field_path))
    ));
    ui.add(
        egui::Label::new(
            RichText::new(
                "На каждой итерации блок получает один текущий элемент, а не весь массив.",
            )
            .size(8.0)
            .color(MUTED),
        )
        .wrap(),
    );
    changed
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(dead_code)]
fn composer_loop_destination_input_editor(
    ui: &mut egui::Ui,
    step_id: &str,
    target: &str,
    manual_value: &mut String,
    loop_source: &ComposerLoopSource,
    bindings: &mut BTreeMap<String, Binding>,
    dark: bool,
) -> bool {
    let suffixes = composer_loop_destination_suffixes(loop_source);
    let existing = bindings.get(target).cloned();
    let mut selection = existing
        .as_ref()
        .and_then(|binding| composer_loop_destination_binding_selection(binding, loop_source));

    if let Some(binding) = existing.as_ref().filter(|_| selection.is_none()) {
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Привязка локальной папки задана в YAML и не поддерживается этим визуальным редактором.",
                )
                .size(8.0)
                .color(ORANGE),
            )
            .wrap(),
        );
        let binding_text = serde_yaml::to_string(binding)
            .unwrap_or_else(|_| "не удалось показать привязку".into())
            .trim()
            .to_owned();
        Frame::new()
            .fill(code_surface(dark))
            .corner_radius(7)
            .inner_margin(Margin::same(7))
            .show(ui, |ui| {
                paint_bounded_code_lines(ui, &binding_text, 8.0, text(dark));
            });
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            if let Some(suffix) = suffixes.first() {
                if ui.button("Путь текущего репозитория").clicked() {
                    bindings.insert(
                        target.into(),
                        composer_loop_destination_binding(loop_source, "$HOME/Developer", suffix),
                    );
                    changed = true;
                }
            }
            if ui.small_button("Заменить ручным вводом").clicked() {
                bindings.remove(target);
                changed = true;
            }
        });
        return changed;
    }

    let mut use_context = selection.is_some();
    let was_context = use_context;
    ui.label(RichText::new("Источник значения").size(8.0).color(MUTED));
    egui::ComboBox::from_id_salt(("loop-destination-mode", step_id, target))
        .selected_text(if use_context {
            "По текущему репозиторию"
        } else {
            "Вручную"
        })
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut use_context, false, "Вручную");
            ui.add_enabled_ui(!suffixes.is_empty(), |ui| {
                ui.selectable_value(&mut use_context, true, "По текущему репозиторию");
            });
        });

    let mut changed = false;
    if use_context != was_context {
        if use_context {
            if let Some(suffix) = suffixes.first() {
                let next = ComposerLoopDestinationBinding {
                    root: "$HOME/Developer".into(),
                    suffix: suffix.clone(),
                };
                bindings.insert(
                    target.into(),
                    composer_loop_destination_binding(loop_source, &next.root, &next.suffix),
                );
                selection = Some(next);
                changed = true;
            }
        } else {
            bindings.remove(target);
            selection = None;
            changed = true;
        }
    }

    if !use_context {
        changed |= ui.text_edit_singleline(manual_value).changed();
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Один и тот же локальный путь будет использован для каждого репозитория в цикле.",
                )
                .size(8.0)
                .color(ORANGE),
            )
            .wrap(),
        );
        if suffixes.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Для динамического пути добавьте full_name либо owner и name в поля контекста For each.",
                    )
                    .size(8.0)
                    .color(MUTED),
                )
                .wrap(),
            );
        }
        return changed;
    }

    let Some(mut selection) = selection else {
        ui.label(
            RichText::new("Не удалось прочитать путь текущего репозитория.")
                .size(8.0)
                .color(ORANGE),
        );
        return changed;
    };

    ui.label(RichText::new("Базовый каталог").size(8.0).color(MUTED));
    if ui.text_edit_singleline(&mut selection.root).changed() {
        changed = true;
    }
    if let Some(error) = composer_destination_root_error(&selection.root) {
        ui.label(RichText::new(error).size(8.0).color(ORANGE));
    }

    ui.label(RichText::new("Подпапка репозитория").size(8.0).color(MUTED));
    let suffix_label = composer_loop_destination_suffix_label(loop_source, &selection.suffix);
    egui::ComboBox::from_id_salt(("loop-destination-suffix", step_id, target))
        .selected_text(&suffix_label)
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            for suffix in &suffixes {
                let label = composer_loop_destination_suffix_label(loop_source, suffix);
                if ui
                    .selectable_label(*suffix == selection.suffix, label)
                    .clicked()
                {
                    selection.suffix = suffix.clone();
                    changed = true;
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text(suffix_label);

    if changed {
        bindings.insert(
            target.into(),
            composer_loop_destination_binding(loop_source, &selection.root, &selection.suffix),
        );
    }
    let preview = composer_loop_destination_preview(loop_source, &selection);
    ui.add(
        egui::Label::new(RichText::new(&preview).monospace().size(8.0).color(PURPLE)).truncate(),
    )
    .on_hover_text(preview);
    ui.add(
        egui::Label::new(
            RichText::new("На каждой итерации путь собирается для одного текущего репозитория.")
                .size(8.0)
                .color(MUTED),
        )
        .wrap(),
    );
    changed
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(dead_code)]
fn composer_indexed_input_editor(
    ui: &mut egui::Ui,
    step_id: &str,
    target: &str,
    label: &str,
    manual_value: &mut String,
    expected: Option<&FieldSchema>,
    array_sources: &[ComposerArraySource],
    bindings: &mut BTreeMap<String, Binding>,
    dark: bool,
) -> bool {
    ui.label(RichText::new(label).size(9.0).color(MUTED));
    let Some(expected) = expected else {
        ui.label(
            RichText::new("У блока нет типизированного контракта для этого входа.")
                .size(8.0)
                .color(ORANGE),
        );
        return false;
    };

    let compatible_sources = array_sources
        .iter()
        .filter_map(|source| {
            let fields = composer_indexed_field_options(source, expected);
            (!fields.is_empty()).then_some((source, fields))
        })
        .collect::<Vec<_>>();
    let existing = bindings.get(target).cloned();
    let parsed = existing
        .as_ref()
        .and_then(|binding| composer_indexed_binding_selection(binding, array_sources));
    let supported = parsed.as_ref().filter(|selection| {
        selection.index < COMPOSER_MAX_ARRAY_ORDINAL
            && compatible_sources.iter().any(|(source, fields)| {
                source.step_id == selection.source_step
                    && source.path == selection.array_path
                    && fields
                        .iter()
                        .any(|field| field.path == selection.field_path)
            })
    });

    if let Some(binding) = existing.as_ref().filter(|_| supported.is_none()) {
        ui.label(
            RichText::new("Привязка задана в YAML и не поддерживается этим визуальным редактором.")
                .size(8.0)
                .color(ORANGE),
        );
        let binding_text = match binding {
            Binding::Field { field } => field_ref_label(field),
            _ => serde_yaml::to_string(binding)
                .unwrap_or_else(|_| "не удалось показать привязку".into())
                .trim()
                .to_owned(),
        };
        Frame::new()
            .fill(code_surface(dark))
            .corner_radius(7)
            .inner_margin(Margin::same(7))
            .show(ui, |ui| {
                paint_bounded_code_lines(ui, &binding_text, 8.0, text(dark));
            });
        if ui.small_button("Заменить ручным вводом").clicked() {
            bindings.remove(target);
            return true;
        }
        return false;
    }

    let mut use_context = supported.is_some();
    let was_context = use_context;
    ui.label(RichText::new("Источник значения").size(8.0).color(MUTED));
    egui::ComboBox::from_id_salt(("input-mode", step_id, target))
        .selected_text(if use_context {
            "Из контекста"
        } else {
            "Вручную"
        })
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut use_context, false, "Вручную");
            ui.add_enabled_ui(!compatible_sources.is_empty(), |ui| {
                ui.selectable_value(&mut use_context, true, "Из контекста");
            });
        });

    let mut changed = false;
    if use_context != was_context {
        if use_context {
            if let Some((source, fields)) = compatible_sources.first() {
                bindings.insert(
                    target.into(),
                    composer_indexed_binding(source, 0, &fields[0].path),
                );
                changed = true;
            }
        } else {
            bindings.remove(target);
            changed = true;
        }
    }

    if !use_context {
        changed |= ui.text_edit_singleline(manual_value).changed();
        if compatible_sources.is_empty() {
            ui.label(
                RichText::new("В предыдущих массивах нет поля совместимого типа.")
                    .size(8.0)
                    .color(MUTED),
            );
        }
        return changed;
    }

    let Some(mut selection) = bindings
        .get(target)
        .and_then(|binding| composer_indexed_binding_selection(binding, array_sources))
    else {
        ui.label(
            RichText::new("Не удалось прочитать выбранную контекстную привязку.")
                .size(8.0)
                .color(ORANGE),
        );
        return changed;
    };

    ui.label(
        RichText::new("Массив предыдущего блока")
            .size(8.0)
            .color(MUTED),
    );
    let source_label = compatible_sources
        .iter()
        .find(|(source, _)| {
            source.step_id == selection.source_step && source.path == selection.array_path
        })
        .map(|(source, _)| {
            format!(
                "{} ({}) → {}[]",
                truncate(&source.step_name, 24),
                source.step_id,
                source.path
            )
        })
        .unwrap_or_else(|| "Источник не выбран".into());
    let source_response = egui::ComboBox::from_id_salt(("indexed-input-source", step_id, target))
        .selected_text(source_label.clone())
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            for (source, fields) in &compatible_sources {
                let selected =
                    source.step_id == selection.source_step && source.path == selection.array_path;
                let label = format!(
                    "{} ({}) → {}[]",
                    truncate(&source.step_name, 24),
                    source.step_id,
                    source.path
                );
                if ui
                    .add(
                        egui::Button::selectable(selected, label)
                            .truncate()
                            .min_size(Vec2::new(ui.available_width(), 0.0)),
                    )
                    .clicked()
                {
                    selection.source_step = source.step_id.clone();
                    selection.array_path = source.path.clone();
                    selection.field_path = fields[0].path.clone();
                    changed = true;
                    ui.close();
                }
            }
        });
    source_response.response.on_hover_text(source_label);

    ui.label(RichText::new("Номер элемента (с 1)").size(8.0).color(MUTED));
    let mut ordinal = selection.index.saturating_add(1);
    if ui
        .add(
            egui::DragValue::new(&mut ordinal)
                .range(1..=COMPOSER_MAX_ARRAY_ORDINAL)
                .speed(1),
        )
        .changed()
    {
        selection.index = ordinal.saturating_sub(1);
        changed = true;
    }

    let selected_source = compatible_sources.iter().find(|(source, _)| {
        source.step_id == selection.source_step && source.path == selection.array_path
    });
    let selected_fields = selected_source
        .map(|(_, fields)| fields.as_slice())
        .unwrap_or_default();
    ui.label(RichText::new("Поле элемента").size(8.0).color(MUTED));
    let field_label = selected_fields
        .iter()
        .find(|field| field.path == selection.field_path)
        .map(|field| {
            let name = if field.path.is_empty() {
                "Элемент целиком"
            } else {
                &field.path
            };
            format!(
                "{} · {}",
                name,
                context_type_label(&field.value_type, field.nullable, !field.required)
            )
        })
        .unwrap_or_else(|| "Поле не выбрано".into());
    let field_response = egui::ComboBox::from_id_salt(("indexed-input-field", step_id, target))
        .selected_text(field_label.clone())
        .width(ui.available_width())
        .truncate()
        .show_ui(ui, |ui| {
            for field in selected_fields {
                let name = if field.path.is_empty() {
                    "Элемент целиком"
                } else {
                    &field.path
                };
                let label = format!(
                    "{} · {}",
                    name,
                    context_type_label(&field.value_type, field.nullable, !field.required)
                );
                if ui
                    .add(
                        egui::Button::selectable(field.path == selection.field_path, label)
                            .truncate()
                            .min_size(Vec2::new(ui.available_width(), 0.0)),
                    )
                    .clicked()
                {
                    selection.field_path = field.path.clone();
                    changed = true;
                    ui.close();
                }
            }
        });
    field_response.response.on_hover_text(field_label);

    if changed {
        if let Some((source, _)) = selected_source {
            bindings.insert(
                target.into(),
                composer_indexed_binding(source, selection.index, &selection.field_path),
            );
        }
    }
    let preview = composer_indexed_binding_preview(&selection, target);
    ui.add(
        egui::Label::new(RichText::new(&preview).monospace().size(8.0).color(PURPLE)).truncate(),
    )
    .on_hover_text(preview);
    ui.add(
        egui::Label::new(
            RichText::new(format!(
                "Длина массива определяется только при запуске. Если в нём меньше {} элементов, входной контекст отсутствует и выполнение остановится (Missing).",
                selection.index.saturating_add(1)
            ))
            .size(8.0)
            .color(ORANGE),
        )
        .wrap(),
    );
    changed
}

#[cfg(test)]
#[allow(dead_code)]
fn composer_binding_selector(
    ui: &mut egui::Ui,
    step_id: &str,
    label: &str,
    value: &mut String,
    options: &[(String, String)],
) -> bool {
    ui.label(RichText::new(label).size(9.0).color(MUTED));
    if options.is_empty() {
        ui.label(
            RichText::new("Нет выбранного совместимого поля в контексте цикла")
                .size(8.0)
                .color(ORANGE),
        );
        return false;
    }
    let selected = options
        .iter()
        .find(|(_, template)| template == value)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "Выберите поле контекста".into());
    let mut changed = false;
    egui::ComboBox::from_id_salt(("context-binding", step_id, label))
        .selected_text(selected)
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for (name, template) in options {
                if ui.selectable_label(template == value, name).clicked() {
                    *value = template.clone();
                    changed = true;
                    ui.close();
                }
            }
        });
    changed
}

#[cfg(test)]
#[allow(dead_code)]
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

fn paint_graph_connector(
    painter: &egui::Painter,
    from: Pos2,
    to: Pos2,
    kind: ComposerGraphEdgeKind,
    port: Option<&EdgePort>,
) {
    let color = match kind {
        ComposerGraphEdgeKind::Flow => PURPLE,
        ComposerGraphEdgeKind::Iteration => CYAN,
        ComposerGraphEdgeKind::Then => CYAN,
        ComposerGraphEdgeKind::Else => ORANGE,
        ComposerGraphEdgeKind::Case => BLUE,
        ComposerGraphEdgeKind::Default => MUTED,
    };
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
        Stroke::new(4.0, translucent(color, 125)),
    ));
    painter.circle_filled(from, 6.0, color);
    painter.circle_filled(to, 6.0, color);
    let label = match kind {
        ComposerGraphEdgeKind::Flow => port.map(|port| graph_edge_port_label(port).to_owned()),
        ComposerGraphEdgeKind::Iteration => Some("для item".into()),
        ComposerGraphEdgeKind::Then => Some("да".into()),
        ComposerGraphEdgeKind::Else => Some("нет".into()),
        ComposerGraphEdgeKind::Case => Some("вариант".into()),
        ComposerGraphEdgeKind::Default => Some("иначе".into()),
    };
    if let Some(label) = label {
        painter.text(
            from.lerp(to, 0.5) + Vec2::new(0.0, -12.0),
            Align2::CENTER_BOTTOM,
            label,
            FontId::monospace(8.0),
            color,
        );
    }
}

fn paint_graph_plus(painter: &egui::Painter, rect: Rect, label: &str) {
    painter.circle_filled(rect.center(), 12.0, PURPLE);
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(16.0),
        Color32::WHITE,
    );
}

fn paint_graph_control_node(
    painter: &egui::Painter,
    rect: Rect,
    id: &str,
    card_kind: &ComposerGraphCard,
    selected: bool,
    status: Option<&StepStatus>,
    dark: bool,
) {
    let (eyebrow, title, icon, accent) = match card_kind {
        ComposerGraphCard::ForEach { item_alias, .. } => {
            ("ЦИКЛ", format!("Для каждого {item_alias}"), "∀", CYAN)
        }
        ComposerGraphCard::If => ("УСЛОВИЕ", "Если / иначе".into(), "?", ORANGE),
        ComposerGraphCard::Switch => ("ВЫБОР", "Switch".into(), "≡", ORANGE),
        ComposerGraphCard::Join => ("СЛИЯНИЕ", "Join".into(), "⋈", BLUE),
        ComposerGraphCard::Action(_) => unreachable!("actions use paint_step_node"),
    };
    painter.rect_filled(
        rect.translate(Vec2::new(0.0, 7.0)),
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
        icon,
        FontId::proportional(15.0),
        accent,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 18.0),
        Align2::LEFT_TOP,
        eyebrow,
        FontId::proportional(8.0),
        MUTED,
    );
    painter.text(
        rect.min + Vec2::new(70.0, 36.0),
        Align2::LEFT_TOP,
        truncate(&title, 23),
        FontId::proportional(13.0),
        text(dark),
    );
    painter.text(
        rect.min + Vec2::new(20.0, 78.0),
        Align2::LEFT_TOP,
        truncate(id, 27),
        FontId::monospace(9.0),
        MUTED,
    );
    paint_status_badge(painter, rect, status);
}

fn graph_control_summary(ui: &mut egui::Ui, kind: &str, id: &str, dark: bool) {
    ui.label(RichText::new(kind).strong().size(10.0).color(CYAN));
    ui.label(RichText::new("ID блока").size(9.0).color(MUTED));
    ui.label(RichText::new(id).monospace().size(9.0).color(PURPLE));
    ui.label(
        RichText::new("Управляющий узел хранит ветви непосредственно в WorkflowGraph.")
            .size(8.0)
            .color(text(dark)),
    );
}

fn binding_label(binding: &Binding) -> String {
    match binding {
        Binding::Field { field } => field_ref_label(field),
        Binding::Literal { value } => serde_json::to_string(value).unwrap_or_else(|_| "?".into()),
        Binding::Interpolated { .. } => "interpolated expression".into(),
        Binding::Template { template } => template.clone(),
    }
}

fn collect_context_type_lines(value_type: &ContextType, path: &str, lines: &mut Vec<String>) {
    match value_type {
        ContextType::Object { schema } => {
            for (name, field) in &schema.fields {
                let child = join_context_path(path, name);
                collect_context_type_lines(&field.value_type, &child, lines);
            }
        }
        ContextType::Array { items } => {
            lines.push(format!(
                "{path}[] : {}",
                context_type_label(items, false, false)
            ));
        }
        _ => lines.push(format!(
            "{path} : {}",
            context_type_label(value_type, false, false)
        )),
    }
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
    task: &Task,
    positions: &BTreeMap<String, Pos2>,
    parents: &BTreeMap<String, String>,
    node_size: Vec2,
) {
    for (child, parent) in parents {
        if !composer_canvas_edge_is_visible(task, child, parent) {
            continue;
        }
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
    let mut x = (rect.left() / step).floor() * step;
    while x <= rect.right() {
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, grid),
        );
        x += step;
    }
    let mut y = (rect.top() / step).floor() * step;
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
        Action::ForEach { .. } => CYAN,
        Action::ForEachGitCloneIfMissing { .. } => PURPLE,
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
        Action::ForEach { .. } => "∀",
        Action::ForEachGitCloneIfMissing { .. } => "⌘",
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
        Action::ForEach { .. } => "Цикл",
        Action::ForEachGitCloneIfMissing { .. } => "Клонирование в цикле",
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
    let supports = |step: &Step| {
        matches!(step.auth, AuthPolicy::None) && action_supports_gui_run(&step.action)
    };
    task.steps.iter().all(&supports)
        && task
            .graph
            .as_ref()
            .is_none_or(|graph| graph_steps_all(graph, &supports))
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
    let unready = |step: &Step| {
        matches!(step.auth, AuthPolicy::GitCredential)
            && matches!(
                &step.action,
                Action::GitClone { repo, .. }
                    | Action::GitInspect { repo, .. }
                    | Action::GitCloneIfMissing { repo, .. }
                    | Action::ForEachGitCloneIfMissing { repo, .. }
                    | Action::GitFetch { repo, .. }
                    | Action::GitFastForward { repo, .. }
                    if !git_clone_auth_ready(repo)
            )
    };
    task.steps.iter().any(&unready)
        || task
            .graph
            .as_ref()
            .is_some_and(|graph| graph_steps_any(graph, &unready))
}

fn task_contains_action(task: &Task, predicate: &dyn Fn(&Action) -> bool) -> bool {
    task.steps.iter().any(|step| predicate(&step.action))
        || task
            .graph
            .as_ref()
            .is_some_and(|graph| graph_steps_any(graph, &|step| predicate(&step.action)))
}

fn graph_steps_all(graph: &WorkflowGraph, predicate: &dyn Fn(&Step) -> bool) -> bool {
    graph.nodes.iter().all(|node| match node {
        GraphNode::Action(node) => predicate(&node.step),
        GraphNode::ForEach(node) => graph_steps_all(&node.body, predicate),
        GraphNode::If(node) => {
            graph_steps_all(&node.then_graph, predicate)
                && node
                    .else_graph
                    .as_deref()
                    .is_none_or(|graph| graph_steps_all(graph, predicate))
        }
        GraphNode::Switch(node) => {
            node.cases
                .iter()
                .all(|case| graph_steps_all(&case.graph, predicate))
                && node
                    .default
                    .as_deref()
                    .is_none_or(|graph| graph_steps_all(graph, predicate))
        }
        GraphNode::Join(_) => true,
    })
}

fn graph_steps_any(graph: &WorkflowGraph, predicate: &dyn Fn(&Step) -> bool) -> bool {
    graph.nodes.iter().any(|node| match node {
        GraphNode::Action(node) => predicate(&node.step),
        GraphNode::ForEach(node) => graph_steps_any(&node.body, predicate),
        GraphNode::If(node) => {
            graph_steps_any(&node.then_graph, predicate)
                || node
                    .else_graph
                    .as_deref()
                    .is_some_and(|graph| graph_steps_any(graph, predicate))
        }
        GraphNode::Switch(node) => {
            node.cases
                .iter()
                .any(|case| graph_steps_any(&case.graph, predicate))
                || node
                    .default
                    .as_deref()
                    .is_some_and(|graph| graph_steps_any(graph, predicate))
        }
        GraphNode::Join(_) => false,
    })
}

fn action_supports_gui_run(action: &Action) -> bool {
    match action {
        Action::ActivateLicense(_)
        | Action::AppStoreInstall(_)
        | Action::RunScript { .. }
        | Action::ConfigurePackageRegistryFiles { .. } => false,
        Action::GithubListRepositories
        | Action::ForEach { .. }
        | Action::ForEachGitCloneIfMissing { .. }
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

fn paint_composer_output_json(ui: &mut egui::Ui, step_id: &str, json: &str, dark: bool) {
    let outer_width = ui.available_width().max(0.0);
    ScrollArea::vertical()
        .id_salt(("composer-step-output", step_id))
        .max_width(outer_width)
        .max_height(240.0)
        .horizontal_scroll_offset(0.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let content_width = ui.available_width().max(0.0);
            ui.set_width(content_width);
            Frame::new()
                .fill(code_surface(dark))
                .corner_radius(8)
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width().max(0.0));
                    paint_bounded_code_lines(ui, json, 8.0, text(dark));
                });
        });
}

fn paint_composer_run_report(
    ui: &mut egui::Ui,
    report: &RunReport,
    selected_step: Option<usize>,
    applied: bool,
    dark: bool,
) {
    ui.add_space(10.0);
    let failed = !report.errors.is_empty();
    ui.label(
        RichText::new(if applied {
            if failed {
                format!(
                    "Выполнение завершилось с ошибкой · шагов: {}",
                    report.steps.len()
                )
            } else {
                format!("Выполнено шагов: {}", report.steps.len())
            }
        } else if failed {
            format!("План содержит ошибок: {}", report.errors.len())
        } else {
            format!("План готов: {} шагов", report.steps.len())
        })
        .strong()
        .size(9.0)
        .color(if failed { ORANGE } else { CYAN }),
    );

    if failed {
        ui.add_space(8.0);
        section_label(ui, "ОШИБКИ ВЫПОЛНЕНИЯ");
        for error in &report.errors {
            error_box(ui, error, dark);
            ui.add_space(6.0);
        }
    }

    let Some(step) = selected_step.and_then(|index| report.steps.get(index)) else {
        return;
    };
    ui.add_space(8.0);
    section_label(ui, "РЕЗУЛЬТАТ ВЫБРАННОГО БЛОКА");
    ui.add(
        egui::Label::new(RichText::new(&step.summary).size(9.0).color(
            if matches!(&step.status, StepStatus::Failed) {
                ORANGE
            } else {
                text(dark)
            },
        ))
        .truncate(),
    )
    .on_hover_text(&step.summary);

    if !step.logs.is_empty() {
        egui::CollapsingHeader::new(format!("Логи блока · {}", step.logs.len()))
            .default_open(matches!(&step.status, StepStatus::Failed))
            .show(ui, |ui| {
                for log in &step.logs {
                    paint_bounded_code_lines(ui, &log.message, 8.0, MUTED);
                }
            });
    }

    if let Some(output) = &step.output {
        let json = serde_json::to_string_pretty(output)
            .unwrap_or_else(|error| format!("Не удалось вывести контекст: {error}"));
        egui::CollapsingHeader::new("Выходной контекст JSON")
            .default_open(false)
            .show(ui, |ui| {
                paint_composer_output_json(ui, &step.step_id, &json, dark);
            });
    }
}

fn github_report_needs_authorization(report: &RunReport) -> bool {
    github_errors_need_authorization(&report.errors)
}

fn github_errors_need_authorization(errors: &[String]) -> bool {
    errors.iter().any(|error| {
        let error = error.to_ascii_lowercase();
        error.contains("github cli")
            && (error.contains("not authenticated")
                || error.contains("is not logged")
                || error.contains("gh auth login"))
    })
}

fn error_box(ui: &mut egui::Ui, error: &str, dark: bool) {
    let red = Color32::from_rgb(194, 64, 64);
    Frame::new()
        .fill(translucent(red, if dark { 36 } else { 16 }))
        .stroke(Stroke::new(1.0, translucent(red, 95)))
        .corner_radius(9)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            for line in error.split('\n') {
                ui.add(egui::Label::new(RichText::new(line).size(9.0).color(red)).truncate());
            }
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

#[cfg(target_os = "macos")]
fn install_unicode_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    use egui::{FontData, FontFamily};

    let fonts = [
        (
            "macos-system-ui",
            "/System/Library/Fonts/SFNS.ttf",
            vec![InsertFontFamily {
                family: FontFamily::Proportional,
                // Preserve egui's original metrics and use SF only for glyphs
                // missing from the built-in proportional font.
                priority: FontPriority::Lowest,
            }],
        ),
        (
            "macos-system-mono",
            "/System/Library/Fonts/SFNSMono.ttf",
            vec![InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            }],
        ),
        (
            "macos-symbols",
            "/System/Library/Fonts/Apple Symbols.ttf",
            vec![
                InsertFontFamily {
                    family: FontFamily::Proportional,
                    priority: FontPriority::Lowest,
                },
                InsertFontFamily {
                    family: FontFamily::Monospace,
                    priority: FontPriority::Lowest,
                },
            ],
        ),
    ];

    for (name, path, families) in fonts {
        if let Ok(bytes) = fs::read(path) {
            ctx.add_font(FontInsert::new(name, FontData::from_owned(bytes), families));
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn install_unicode_fonts(_ctx: &egui::Context) {}

fn configure_styles(ctx: &egui::Context, preference: egui::ThemePreference) {
    for (theme, dark) in [(egui::Theme::Light, false), (egui::Theme::Dark, true)] {
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
    ctx.set_theme(preference);
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

    /// Legacy linear fixture retained only to exercise the import-only UI
    /// compatibility helpers. Production authoring uses
    /// `github_repository_composer_task`, which is graph-native.
    fn legacy_github_repository_composer_task(ordinal: usize) -> Task {
        Task {
            id: format!("github-repositories-{ordinal}"),
            name: "Получить репозитории GitHub".into(),
            description: "Legacy test fixture".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            graph: None,
            steps: vec![composer_step(
                ComposerBlockKind::GithubListRepositories,
                "list-repositories".into(),
            )],
        }
    }

    fn composer_project_with_canvas(task: Task, canvas: ComposerCanvas) -> ScenarioProject {
        ScenarioProject {
            id: "test-project".into(),
            name: "Test project".into(),
            description: String::new(),
            canvases: BTreeMap::from([(task.id.clone(), canvas)]),
            entries: vec![ProjectEntry::Scenario {
                task: Box::new(task),
            }],
        }
    }

    fn composer_app_for_test(project: ScenarioProject) -> ScenarioApp {
        ScenarioApp {
            task_pack: None,
            load_error: None,
            selected_task: 0,
            selected_step: None,
            selected_node: None,
            channel: ReleaseChannel::Release,
            allow_shell: false,
            allow_elevation: false,
            report: None,
            report_applied: false,
            plan_error: None,
            dark: true,
            confirm_run: false,
            running: false,
            run_receiver: None,
            github_picker: GithubPickerState::default(),
            file_message: None,
            custom_project: Some(project),
            selected_project_scenario: Some(vec![0]),
            selected_project_group: Vec::new(),
            block_picker_parent: None,
            graph_picker_attach: None,
            graph_picker_port: None,
            block_picker_search: String::new(),
            readonly_canvas_views: BTreeMap::new(),
        }
    }

    #[test]
    fn compact_workspace_keeps_canvas_and_inspector_usable() {
        let viewport_width = 1012.0;
        let (library_width, inspector_width) = workspace_panel_widths(viewport_width);
        let canvas_width = viewport_width - library_width - inspector_width;

        assert!(library_width <= 230.0);
        assert!(inspector_width >= 340.0);
        assert!(canvas_width >= 440.0);

        assert_eq!(
            workspace_panel_widths(WIDE_VIEWPORT_WIDTH),
            (WIDE_LIBRARY_WIDTH, WIDE_INSPECTOR_WIDTH)
        );
    }

    fn assert_pos_close(actual: Pos2, expected: Pos2) {
        assert!(
            actual.distance(expected) < 0.001,
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn canvas_view_transforms_round_trip_and_scale_drag_deltas() {
        let viewport = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(800.0, 600.0));
        let view = CanvasView {
            pan: CanvasPoint { x: 40.0, y: -20.0 },
            zoom: 2.0,
        };
        let world = Pos2::new(12.0, 30.0);
        let screen = view.world_to_screen(viewport, world);

        assert_pos_close(screen, Pos2::new(164.0, 90.0));
        assert_pos_close(view.screen_to_world(viewport, screen), world);
        assert_eq!(
            view.screen_rect(viewport, Rect::from_min_size(world, Vec2::new(20.0, 10.0)))
                .size(),
            Vec2::new(40.0, 20.0)
        );
        let world_after_drag = view.screen_to_world(viewport, screen + Vec2::new(20.0, -10.0));
        assert_pos_close(world_after_drag, world + Vec2::new(10.0, -5.0));

        let half_scale = CanvasView {
            zoom: 0.5,
            ..CanvasView::default()
        };
        let origin = half_scale.screen_to_world(viewport, viewport.min);
        let after = half_scale.screen_to_world(viewport, viewport.min + Vec2::new(20.0, -10.0));
        assert_pos_close(after, origin + Vec2::new(40.0, -20.0));
    }

    #[test]
    fn canvas_zoom_keeps_anchor_fixed_and_clamps() {
        let viewport = Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(900.0, 600.0));
        let anchor = Pos2::new(410.0, 275.0);
        let mut view = CanvasView {
            pan: CanvasPoint { x: -75.0, y: 34.0 },
            zoom: 0.8,
        };
        let anchor_world = view.screen_to_world(viewport, anchor);

        view.zoom_about(viewport, anchor, 100.0);
        assert_eq!(view.zoom, CANVAS_MAX_ZOOM);
        assert_pos_close(view.world_to_screen(viewport, anchor_world), anchor);

        view.zoom_about(viewport, anchor, 0.0001);
        assert_eq!(view.zoom, CANVAS_MIN_ZOOM);
        assert_pos_close(view.world_to_screen(viewport, anchor_world), anchor);
    }

    #[test]
    fn canvas_pan_and_fit_are_viewport_relative() {
        let viewport = Rect::from_min_size(Pos2::new(200.0, 80.0), Vec2::new(1000.0, 600.0));
        let content = Rect::from_min_size(Pos2::new(100.0, 120.0), Vec2::new(600.0, 300.0));
        let view = CanvasView::fit(content, viewport, 50.0);
        let fitted = view.screen_rect(viewport, content);
        let safe = viewport.shrink(50.0);

        assert!((view.zoom - 1.5).abs() < 0.001);
        assert!(safe.contains_rect(fitted));
        assert_pos_close(fitted.center(), viewport.center());

        let before = view.world_to_screen(viewport, content.min);
        let mut panned = view;
        panned.pan_by(Vec2::new(35.0, -18.0));
        assert_pos_close(
            panned.world_to_screen(viewport, content.min),
            before + Vec2::new(35.0, -18.0),
        );
    }

    #[test]
    fn canvas_view_serde_is_backward_compatible_and_skips_defaults() {
        let legacy: ComposerCanvas = serde_yaml::from_str(
            r#"
positions:
  start: { x: 80.0, y: 250.0 }
"#,
        )
        .unwrap();
        assert_eq!(legacy.view, CanvasView::default());

        let default_yaml = serde_yaml::to_string(&legacy).unwrap();
        assert!(!default_yaml.contains("view:"));

        let expected = CanvasView {
            pan: CanvasPoint {
                x: -135.5,
                y: 42.25,
            },
            zoom: 1.75,
        };
        let canvas = ComposerCanvas {
            view: expected,
            ..legacy
        };
        let yaml = serde_yaml::to_string(&canvas).unwrap();
        let decoded: ComposerCanvas = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(decoded.view, expected);
    }

    #[test]
    fn canvas_scene_zoom_is_cursor_anchored_headlessly() {
        fn input(size: Vec2, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            }
        }

        fn paint_scene(ui: &mut egui::Ui, scene_rect: &mut Rect, viewport: &mut Rect) {
            *viewport = ui.available_rect_before_wrap();
            egui::Scene::new()
                .zoom_range(CANVAS_MIN_ZOOM..=CANVAS_MAX_ZOOM)
                .show(ui, scene_rect, |ui| {
                    let rect =
                        Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(300.0, 200.0));
                    ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
                    ui.interact(rect, Id::new("scene-probe"), Sense::click());
                });
        }

        let ctx = egui::Context::default();
        let size = Vec2::new(800.0, 600.0);
        let anchor = Pos2::new(420.0, 310.0);
        let mut viewport = Rect::ZERO;
        let mut scene_rect = Rect::from_min_size(Pos2::ZERO, size);

        let mut output = ctx.run_ui(input(size, vec![egui::Event::PointerMoved(anchor)]), |ui| {
            paint_scene(ui, &mut scene_rect, &mut viewport)
        });
        output.textures_delta.clear();
        let before = CanvasView::from_visible_world_rect(scene_rect, viewport)
            .screen_to_world(viewport, anchor);

        let mut output = ctx.run_ui(
            input(
                size,
                vec![egui::Event::PointerMoved(anchor), egui::Event::Zoom(1.5)],
            ),
            |ui| paint_scene(ui, &mut scene_rect, &mut viewport),
        );
        output.textures_delta.clear();
        let after_view = CanvasView::from_visible_world_rect(scene_rect, viewport);
        let after = after_view.screen_to_world(viewport, anchor);

        assert_pos_close(after, before);
        assert!((after_view.zoom - 1.5).abs() < 0.01);
    }

    #[test]
    fn canvas_scene_separates_scaled_node_drag_from_background_pan() {
        fn input(size: Vec2, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            }
        }

        fn button(pos: Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }
        }

        fn frame(
            ctx: &egui::Context,
            size: Vec2,
            scene_rect: &mut Rect,
            node: &mut Pos2,
            events: Vec<egui::Event>,
        ) {
            let mut output = ctx.run_ui(input(size, events), |ui| {
                let mut child_consumed_drag = false;
                let mut background = None;
                egui::Scene::new()
                    .zoom_range(CANVAS_MIN_ZOOM..=CANVAS_MAX_ZOOM)
                    .sense(Sense::hover())
                    .drag_pan_buttons(egui::DragPanButtons::empty())
                    .show(ui, scene_rect, |ui| {
                        background = Some(ui.interact(
                            ui.clip_rect(),
                            Id::new("drag-background"),
                            Sense::click_and_drag(),
                        ));
                        let rect = Rect::from_min_size(*node, Vec2::new(100.0, 60.0));
                        let response = ui.interact(rect, Id::new("drag-node"), Sense::drag());
                        if response.dragged_by(egui::PointerButton::Primary) {
                            child_consumed_drag = true;
                            *node += response.drag_delta();
                        }
                    });
                if let Some(background) = background.filter(|background| {
                    !child_consumed_drag && background.dragged_by(egui::PointerButton::Primary)
                }) {
                    *scene_rect = scene_rect.translate(-background.drag_delta());
                }
            });
            output.textures_delta.clear();
        }

        let size = Vec2::new(800.0, 600.0);
        let initial_scene = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0));

        let ctx = egui::Context::default();
        let mut scene_rect = initial_scene;
        let mut node = Pos2::new(100.0, 100.0);
        let node_start = Pos2::new(250.0, 240.0);
        let node_end = node_start + Vec2::new(40.0, 20.0);
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![egui::Event::PointerMoved(node_start)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![button(node_start, true)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![egui::Event::PointerMoved(node_end)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![button(node_end, false)],
        );

        assert_pos_close(node, Pos2::new(120.0, 110.0));
        assert_pos_close(scene_rect.min, initial_scene.min);
        assert_pos_close(scene_rect.max, initial_scene.max);

        let ctx = egui::Context::default();
        let mut scene_rect = initial_scene;
        let mut node = Pos2::new(100.0, 100.0);
        let background_start = Pos2::new(700.0, 500.0);
        let background_end = background_start + Vec2::new(40.0, 20.0);
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![egui::Event::PointerMoved(background_start)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![button(background_start, true)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![egui::Event::PointerMoved(background_end)],
        );
        frame(
            &ctx,
            size,
            &mut scene_rect,
            &mut node,
            vec![button(background_end, false)],
        );

        assert_pos_close(node, Pos2::new(100.0, 100.0));
        assert_pos_close(scene_rect.min, Pos2::new(-20.0, -10.0));
        assert_pos_close(scene_rect.max, Pos2::new(380.0, 290.0));
    }

    #[test]
    fn graph_composer_canvas_drags_real_node_at_zoom_without_moving_view() {
        fn input(size: Vec2, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            }
        }

        fn button(pos: Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }
        }

        fn render(
            ctx: &egui::Context,
            app: &mut ScenarioApp,
            size: Vec2,
            events: Vec<egui::Event>,
        ) {
            let task = app.selected_task().expect("selected graph task").clone();
            let graph = task.graph.as_ref().expect("v3 graph").clone();
            let canvas = app
                .custom_project
                .as_ref()
                .and_then(|project| project.canvases.get(&task.id))
                .expect("composer canvas")
                .clone();
            let mut output = ctx.run_ui(input(size, events), |ui| {
                app.graph_composer_canvas(ui, &task, &graph, &canvas, true);
            });
            output.textures_delta.clear();
        }

        let task = github_repository_composer_task(1);
        let task_id = task.id.clone();
        let graph = task.graph.as_ref().unwrap();
        let node_id = graph.nodes[0].id().to_owned();
        let mut canvas = default_graph_canvas(graph);
        canvas.view = CanvasView {
            zoom: 1.2,
            ..CanvasView::default()
        };
        let initial_position = canvas.positions[&node_id];
        let initial_view = canvas.view;
        let project = composer_project_with_canvas(task, canvas);
        let mut app = composer_app_for_test(project);
        let ctx = egui::Context::default();
        configure_styles(&ctx, egui::ThemePreference::Dark);
        let size = Vec2::new(1200.0, 800.0);
        let response_id = Id::new(("graph-node", task_id.as_str(), node_id.as_str()));

        // Warm up egui's previous-pass hit map, then prove that the actual
        // production card hit rectangle carries the Scene transform.
        render(&ctx, &mut app, size, Vec::new());
        let response = ctx
            .read_response(response_id)
            .expect("graph card response after warmup");
        let screen_rect = ctx
            .layer_transform_to_global(response.layer_id)
            .expect("Scene layer transform")
            * response.rect;
        assert!(
            (screen_rect.width() - 232.0 * 1.2).abs() < 0.1,
            "unexpected response rect {:?}",
            screen_rect
        );
        assert!(
            (screen_rect.height() - 116.0 * 1.2).abs() < 0.1,
            "unexpected response rect {:?}",
            screen_rect
        );
        let start = screen_rect.center();

        render(&ctx, &mut app, size, vec![egui::Event::PointerMoved(start)]);
        assert!(
            ctx.read_response(response_id)
                .is_some_and(|response| response.contains_pointer()),
            "pointer must hit the transformed production card"
        );
        render(&ctx, &mut app, size, vec![button(start, true)]);
        let midway = start + Vec2::new(36.0, 18.0);
        render(
            &ctx,
            &mut app,
            size,
            vec![egui::Event::PointerMoved(midway)],
        );
        let end = start + Vec2::new(72.0, 36.0);
        render(&ctx, &mut app, size, vec![egui::Event::PointerMoved(end)]);
        render(&ctx, &mut app, size, vec![button(end, false)]);

        let saved = &app.custom_project.as_ref().unwrap().canvases[&task_id];
        let moved = saved.positions[&node_id];
        assert!((moved.x - (initial_position.x + 60.0)).abs() < 0.1);
        assert!((moved.y - (initial_position.y + 30.0)).abs() < 0.1);
        assert_eq!(saved.view, initial_view, "node drag must not pan the view");
    }

    #[test]
    fn canvas_scene_pans_over_nodes_with_middle_or_space_primary_without_double_delta() {
        fn input(size: Vec2, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                events,
                ..Default::default()
            }
        }

        fn pointer(pos: Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::default(),
            }
        }

        fn space(pressed: bool) -> egui::Event {
            egui::Event::Key {
                key: egui::Key::Space,
                physical_key: Some(egui::Key::Space),
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }
        }

        fn frame(ctx: &egui::Context, size: Vec2, scene_rect: &mut Rect, events: Vec<egui::Event>) {
            let mut captured_pan = Vec2::ZERO;
            let mut output = ctx.run_ui(input(size, events), |ui| {
                let mut child_consumed_drag = false;
                let mut background = None;
                egui::Scene::new()
                    .zoom_range(CANVAS_MIN_ZOOM..=CANVAS_MAX_ZOOM)
                    .sense(Sense::hover())
                    .drag_pan_buttons(egui::DragPanButtons::empty())
                    .show(ui, scene_rect, |ui| {
                        background = Some(ui.interact(
                            ui.clip_rect(),
                            Id::new("pan-over-node-background"),
                            Sense::click_and_drag(),
                        ));
                        let response = ui.interact(
                            Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(100.0, 60.0)),
                            Id::new("pan-over-node"),
                            Sense::click_and_drag(),
                        );
                        let space_down = ui.input(|input| input.key_down(egui::Key::Space));
                        if response.dragged_by(egui::PointerButton::Middle)
                            || (space_down && response.dragged_by(egui::PointerButton::Primary))
                        {
                            child_consumed_drag = true;
                            captured_pan += response.drag_delta();
                        }
                    });
                if let Some(background) = background.filter(|background| {
                    !child_consumed_drag
                        && (background.dragged_by(egui::PointerButton::Primary)
                            || background.dragged_by(egui::PointerButton::Middle))
                }) {
                    captured_pan += background.drag_delta();
                }
            });
            if captured_pan != Vec2::ZERO {
                *scene_rect = scene_rect.translate(-captured_pan);
            }
            output.textures_delta.clear();
        }

        let size = Vec2::new(800.0, 600.0);
        let initial = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 300.0));
        let start = Pos2::new(250.0, 240.0);
        let end = start + Vec2::new(40.0, 20.0);

        for (button, use_space) in [
            (egui::PointerButton::Middle, false),
            (egui::PointerButton::Primary, true),
        ] {
            let ctx = egui::Context::default();
            let mut scene_rect = initial;
            let mut hover_events = vec![egui::Event::PointerMoved(start)];
            if use_space {
                hover_events.push(space(true));
            }
            frame(&ctx, size, &mut scene_rect, hover_events);
            frame(
                &ctx,
                size,
                &mut scene_rect,
                vec![pointer(start, button, true)],
            );
            frame(
                &ctx,
                size,
                &mut scene_rect,
                vec![egui::Event::PointerMoved(end)],
            );
            let mut release_events = vec![pointer(end, button, false)];
            if use_space {
                release_events.push(space(false));
            }
            frame(&ctx, size, &mut scene_rect, release_events);

            assert_pos_close(scene_rect.min, Pos2::new(-20.0, -10.0));
            assert_pos_close(scene_rect.max, Pos2::new(380.0, 290.0));
        }
    }

    #[test]
    fn canvas_scene_render_stays_clipped_at_supported_viewports_and_zoom_extremes() {
        let sizes = [
            Vec2::new(1012.0, 680.0),
            Vec2::new(1560.0, 720.0),
            Vec2::new(1920.0, 1080.0),
        ];
        let zooms = [CANVAS_MIN_ZOOM, 1.0, CANVAS_MAX_ZOOM];

        for size in sizes {
            for zoom in zooms {
                let ctx = egui::Context::default();
                let screen = Rect::from_min_size(Pos2::ZERO, size);
                let view = CanvasView {
                    pan: CanvasPoint {
                        x: -317.0,
                        y: 143.0,
                    },
                    zoom,
                };
                let mut scene_rect = view.visible_world_rect(screen);
                let mut viewport = Rect::ZERO;
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ui| {
                        viewport = ui.available_rect_before_wrap();
                        egui::Scene::new()
                            .zoom_range(CANVAS_MIN_ZOOM..=CANVAS_MAX_ZOOM)
                            .show(ui, &mut scene_rect, |ui| {
                                let visible = ui.clip_rect();
                                paint_grid(ui.painter(), visible, true);
                                ui.painter().rect_filled(
                                    visible.expand(400.0),
                                    0.0,
                                    Color32::from_rgb(7, 11, 13),
                                );
                            });
                    },
                );

                assert!(
                    !output.shapes.is_empty(),
                    "scene produced no shapes at {size:?}, zoom {zoom}"
                );
                for clipped in &output.shapes {
                    let visible_shape = clipped
                        .shape
                        .visual_bounding_rect()
                        .intersect(clipped.clip_rect);
                    if visible_shape.is_positive() {
                        assert!(
                            viewport.expand(0.01).contains_rect(visible_shape),
                            "shape escaped {viewport:?} at {size:?}, zoom {zoom}: {visible_shape:?}"
                        );
                    }
                    assert!(
                        viewport.expand(0.01).contains_rect(clipped.clip_rect),
                        "clip escaped {viewport:?} at {size:?}, zoom {zoom}: {:?}",
                        clipped.clip_rect
                    );
                }
                let round_trip = CanvasView::from_visible_world_rect(scene_rect, viewport);
                assert!((round_trip.zoom - zoom).abs() < 0.001);
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn inspector_render_contains_long_schema_bindings_after_stale_horizontal_scroll() {
        #[derive(Clone, Copy)]
        struct InspectorProbe {
            panel: Rect,
            inner: Rect,
            content: Rect,
            clip: Rect,
            content_size: Vec2,
            offset: Vec2,
        }

        fn input(size: Vec2) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                ..Default::default()
            }
        }

        fn render(size: Vec2) -> (egui::FullOutput, InspectorProbe) {
            let ctx = egui::Context::default();
            configure_styles(&ctx, egui::ThemePreference::Dark);
            let huge_token = format!(
                "github.repositories[].https_url/путь/{}",
                "репозиторий_с_юникодом_и_без_пробелов".repeat(80)
            );

            // Reproduce the persisted horizontal offset that caused the
            // inspector to start mid-string after an older wide layout.
            let mut seeded_offset = 0.0;
            let mut seed_output = ctx.run_ui(input(size), |root| {
                let (_, inspector_width) = workspace_panel_widths(size.x);
                egui::Panel::right("inspector-containment-panel")
                    .exact_size(inspector_width)
                    .resizable(false)
                    .show(root, |ui| {
                        let output = ScrollArea::both()
                            .id_salt("inspector-containment-scroll")
                            .horizontal_scroll_offset(900.0)
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(&huge_token).extend());
                            });
                        seeded_offset = output.state.offset.x;
                    });
            });
            seed_output.textures_delta.clear();
            assert!(seeded_offset > 0.0, "test must seed a stale x offset");

            let mut step = default_step(ActionKind::GitClone, format!("clone-{huge_token}"))
                .expect("git clone default");
            step.name = format!("Клонировать {huge_token}");
            let repo = Binding::field(
                FieldRef::loop_item("for-each-repositories-with-a-very-long-owner-id")
                    .field("https_url"),
            );
            let dest = Binding::interpolated([
                TemplatePart::literal("$HOME/Developer/"),
                TemplatePart::field(
                    FieldRef::loop_item("for-each-repositories-with-a-very-long-owner-id")
                        .field("full_name"),
                ),
            ]);
            let mut node = ActionNode {
                step,
                bindings: BTreeMap::from([
                    ("repo".into(), repo.clone()),
                    ("dest".into(), dest.clone()),
                ]),
            };
            let option = |label: String, binding: Binding, value_type: ContextType| {
                ComposerGraphBindingOption {
                    label,
                    binding,
                    value_type,
                    required: true,
                    nullable: false,
                    sensitivity: Sensitivity::Public,
                }
            };
            let options = BTreeMap::from([
                (
                    "repo".into(),
                    vec![option(
                        format!("Текущий элемент цикла → {huge_token}"),
                        repo,
                        ContextType::String {
                            format: Some(SemanticFormat::GitUrl),
                        },
                    )],
                ),
                (
                    "dest".into(),
                    vec![option(
                        format!("Интерполяция папки → {huge_token}"),
                        dest,
                        ContextType::String {
                            format: Some(SemanticFormat::DirectoryPath),
                        },
                    )],
                ),
            ]);
            let source = GraphNode::Action(Box::new(ActionNode {
                step: default_step(ActionKind::InspectPath, format!("source-{huge_token}"))
                    .expect("inspect path default"),
                bindings: BTreeMap::new(),
            }));
            let huge_case_id = format!("case-{huge_token}");
            let mut huge_case_value = serde_json::Value::String(huge_token.clone());
            let mut used_case_values = vec![huge_case_value.clone()];
            let structured_output = StepOutput::Structured(StructuredStepOutput {
                schema_id: "ppduster.test.huge-output@1".into(),
                value: serde_json::json!({
                    "unicode": huge_token.clone(),
                    "nested": { "path": format!("/tmp/{huge_token}") },
                }),
            });
            let report = RunReport {
                task_id: format!("task-{huge_token}"),
                task_name: format!("Сценарий {huge_token}"),
                task_description: huge_token.clone(),
                scenarios: Vec::new(),
                plans: Vec::new(),
                outcomes: Vec::new(),
                steps: vec![StepReport {
                    step_id: format!("report-step-{huge_token}"),
                    step_name: format!("Блок {huge_token}"),
                    summary: format!("Результат {huge_token}"),
                    status: StepStatus::Failed,
                    prerequisites: Vec::new(),
                    logs: vec![StepLogEntry {
                        step_id: format!("report-step-{huge_token}"),
                        message: format!("log::{huge_token}\njson::{huge_token}"),
                    }],
                    output: Some(structured_output),
                }],
                context: ContextStore::default(),
                errors: vec![format!("runtime-error::{huge_token}")],
            };
            let report_json = serde_json::to_string_pretty(
                report.steps[0].output.as_ref().expect("structured output"),
            )
            .expect("serialize structured output");

            let mut probe = None;
            let mut output = ctx.run_ui(input(size), |root| {
                let (_, inspector_width) = workspace_panel_widths(size.x);
                let panel = egui::Panel::right("inspector-containment-panel")
                    .exact_size(inspector_width)
                    .resizable(false)
                    .frame(
                        Frame::new()
                            .fill(surface(true))
                            .stroke(Stroke::new(1.0, line(true)))
                            .inner_margin(Margin::same(16)),
                    )
                    .show(root, |ui| {
                        ui.label(RichText::new("ИНСПЕКТОР").strong().size(10.0).color(MUTED));
                        let scroll =
                            bounded_inspector_scroll(ui, "inspector-containment-scroll", |ui| {
                                paint_graph_action_editor(ui, &mut node, &options, true);
                                section_label(ui, "SWITCH · ДЛИННЫЙ CASE");
                                let _ = paint_switch_case_header(ui, &huge_case_id, 1, 2, true);
                                let _ = paint_switch_case_value_row(
                                    ui,
                                    ("switch-huge", &huge_case_id),
                                    0,
                                    2,
                                    &mut huge_case_value,
                                    Some((GraphSwitchScalarKind::String, false)),
                                    &mut used_case_values,
                                );
                                section_label(ui, "JOIN · ДЛИННЫЙ SOURCE");
                                let _ = paint_join_source_row(
                                    ui,
                                    "join-huge",
                                    &source,
                                    Some(EdgePort::Success),
                                );
                                section_label(ui, "RUNTIME · UNICODE JSON/LOG");
                                paint_composer_run_report(ui, &report, Some(0), true, true);
                                // The production JSON section is collapsed by
                                // default. Render the same bounded body here so
                                // this geometry regression always exercises it.
                                paint_composer_output_json(
                                    ui,
                                    &report.steps[0].step_id,
                                    &report_json,
                                    true,
                                );
                                section_label(ui, "ВЫХОДНОЙ КОНТЕКСТ");
                                for line in schema_context_lines(
                                    &definition_for_action(&node.step.action).output_schema,
                                ) {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("{line}::{huge_token}"))
                                                .monospace()
                                                .size(8.0)
                                                .color(PURPLE),
                                        )
                                        .truncate(),
                                    );
                                }
                                // Exercise a code field with one huge unbroken
                                // Unicode/path token under the inspector clip.
                                Frame::new().fill(code_surface(true)).show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&huge_token).monospace().size(8.0),
                                        )
                                        .truncate(),
                                    );
                                });
                                (ui.min_rect(), ui.clip_rect())
                            });
                        let (content, clip) = scroll.inner;
                        (
                            scroll.inner_rect,
                            content,
                            clip,
                            scroll.content_size,
                            scroll.state.offset,
                        )
                    });
                let (inner, content, clip, content_size, offset) = panel.inner;
                probe = Some(InspectorProbe {
                    panel: panel.response.rect,
                    inner,
                    content,
                    clip,
                    content_size,
                    offset,
                });
            });
            output.textures_delta.clear();
            (output, probe.expect("inspector probe"))
        }

        for size in [
            Vec2::new(1560.0, 720.0),
            Vec2::new(1012.0, 680.0),
            Vec2::new(1920.0, 1080.0),
        ] {
            let (output, probe) = render(size);
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            let tolerance = 1.0;
            assert!(screen.expand(tolerance).contains_rect(probe.panel));
            assert!(probe.panel.expand(tolerance).contains_rect(probe.inner));
            assert!(probe.panel.expand(tolerance).contains_rect(probe.clip));
            assert!(probe.content.left() >= probe.inner.left() - tolerance);
            assert!(probe.content.right() <= probe.inner.right() + tolerance);
            assert!(probe.content_size.x <= probe.inner.width() + tolerance);
            assert!((probe.content.left() - probe.inner.left()).abs() <= tolerance);
            assert_eq!(probe.offset.x, 0.0, "stale horizontal offset must be reset");

            // `content` is the union of all response allocations in the
            // bounded inspector UI. Visible paint bounds and their effective
            // clips must likewise stay in the fixed right panel.
            for clipped in output.shapes {
                let visible = clipped
                    .shape
                    .visual_bounding_rect()
                    .intersect(clipped.clip_rect);
                if visible.is_positive() && visible.intersects(probe.panel) {
                    assert!(
                        probe.panel.expand(tolerance).contains_rect(visible),
                        "paint escaped inspector at {size:?}: {visible:?} outside {:?}",
                        probe.panel
                    );
                }
            }
        }
    }

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
    fn gui_capability_checks_actions_inside_graph_tasks() {
        let mut task = legacy_github_repository_composer_task(1);
        let unsupported = Step {
            id: "script".into(),
            name: "Script".into(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: Default::default(),
            when: None,
            require: None,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "script.sh".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                success_exit_codes: vec![0],
            },
        };
        task.steps.clear();
        task.graph = Some(WorkflowGraph {
            entries: vec![unsupported.id.clone()],
            nodes: vec![GraphNode::Action(Box::new(
                ppduster::automation::ActionNode {
                    step: unsupported,
                    bindings: BTreeMap::new(),
                },
            ))],
            ..WorkflowGraph::default()
        });

        assert!(!task_supports_gui_run(&task));
        assert!(task_contains_action(&task, &|action| matches!(
            action,
            Action::RunScript { .. }
        )));
        assert!(graph_steps_any(
            task.graph.as_ref().unwrap(),
            &|step| matches!(step.action, Action::RunScript { .. })
        ));
    }

    #[test]
    fn join_source_port_editing_preserves_explicit_graph_ports() {
        let source = default_step(ActionKind::GitInspect, "source").unwrap();
        let other = default_step(ActionKind::InspectPath, "other").unwrap();
        let mut graph = WorkflowGraph {
            entries: vec!["source".into(), "other".into()],
            nodes: vec![
                GraphNode::Action(Box::new(ActionNode {
                    step: source,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::Action(Box::new(ActionNode {
                    step: other,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::Join(JoinNode {
                    id: "join".into(),
                    mode: JoinMode::All,
                }),
            ],
            edges: vec![
                GraphEdge::new("source", EdgePort::Failure, "join"),
                GraphEdge::new("other", EdgePort::Success, "join"),
            ],
            ..WorkflowGraph::default()
        };

        graph_set_incoming_edge(&mut graph, "join", "source", Some(EdgePort::Always)).unwrap();
        assert!(graph.edges.iter().any(|edge| {
            edge.from.node == "source"
                && edge.from.port == EdgePort::Always
                && edge.to.node == "join"
        }));
        assert!(!graph
            .edges
            .iter()
            .any(|edge| { edge.from.node == "source" && edge.from.port == EdgePort::Failure }));

        graph_set_incoming_edge(&mut graph, "join", "source", None).unwrap();
        assert!(!graph
            .edges
            .iter()
            .any(|edge| edge.from.node == "source" && edge.to.node == "join"));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.from.node == "other" && edge.to.node == "join"));
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
            task_action_steps(&resolved).len()
        );
        assert!(groups.iter().all(|group| !group.description.is_empty()));
        assert!(groups
            .iter()
            .all(|group| group.step_summaries.len() == group.step_count));
        assert!(task_action_steps(&resolved).len() > groups.len());
    }

    #[test]
    fn inspector_describes_every_resolved_step() {
        let pack = load_tasks().unwrap();
        let resolved = pack.resolve("macos-developer-workstation").unwrap();
        let summaries = describe_task_steps(&resolved, &RunOptions::default());

        assert_eq!(summaries.len(), task_action_steps(&resolved).len());
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

        assert!(configured.steps.is_empty());
        let configured_steps = task_action_steps(&configured);
        assert_eq!(configured_steps.len(), 8);
        assert!(configured_steps[0]
            .id
            .starts_with("inspect-repository/acme-api-"));
        assert!(configured_steps[4]
            .id
            .starts_with("inspect-repository/zeta-api-"));
        assert_ne!(configured_steps[0].id, configured_steps[1].id);
        assert!(configured_steps
            .iter()
            .all(|step| matches!(step.auth, AuthPolicy::None)));
        assert!(matches!(
            &configured_steps[0].action,
            Action::GitInspect { repo, dest }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
        ));
        assert!(matches!(
            &configured_steps[1].action,
            Action::GitCloneIfMissing { repo, dest, branch }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch.as_deref() == Some("main")
        ));
        assert!(matches!(
            &configured_steps[2].action,
            Action::GitFetch { repo, dest, branch }
                if repo == "https://github.com/acme/api.git"
                    && dest == "/tmp/workspaces/acme/api"
                    && branch == "main"
        ));
        assert!(matches!(
            &configured_steps[3].action,
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
            graph: None,
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
        let task = task.into_v3().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        let reparsed: TaskFile = serde_yaml::from_str(&yaml).unwrap();
        assert!(reparsed.task.steps.is_empty());
        let actions = task_action_steps(&reparsed.task);
        assert_eq!(actions.len(), 4);
        assert!(matches!(actions[0].action, Action::GitInspect { .. }));
        assert!(matches!(
            actions[1].action,
            Action::GitCloneIfMissing { .. }
        ));
        assert!(matches!(actions[2].action, Action::GitFetch { .. }));
        assert!(matches!(actions[3].action, Action::GitFastForward { .. }));
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
            graph: None,
            steps: vec![composer_step(
                ComposerBlockKind::GitInspect,
                "inspect".into(),
            )],
        };
        let task = task.into_v3().unwrap();
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
                    entries: vec![ProjectEntry::Scenario {
                        task: Box::new(task),
                    }],
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
        let inspect_a = composer_step(ComposerBlockKind::InspectPath, "inspect-a".into());
        let inspect_b = composer_step(ComposerBlockKind::InspectPath, "inspect-b".into());
        let task = Task {
            id: "branched-scenario".into(),
            name: "Branched scenario".into(),
            description: "Two blocks attached to Start.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            graph: Some(WorkflowGraph {
                entries: vec!["inspect-a".into(), "inspect-b".into()],
                nodes: vec![
                    GraphNode::Action(Box::new(ActionNode {
                        step: inspect_a,
                        bindings: BTreeMap::new(),
                    })),
                    GraphNode::Action(Box::new(ActionNode {
                        step: inspect_b,
                        bindings: BTreeMap::new(),
                    })),
                ],
                ..WorkflowGraph::default()
            }),
            steps: Vec::new(),
        };
        let project = ScenarioProject {
            id: "branched-project".into(),
            name: "Branched project".into(),
            description: String::new(),
            entries: vec![ProjectEntry::Scenario {
                task: Box::new(task),
            }],
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
                    view: CanvasView::default(),
                },
            )]),
        };

        let yaml = serde_yaml::to_string(&ScenarioProjectFile { project }).unwrap();
        let reparsed = load_project_yaml(&yaml).unwrap();
        let canvas = &reparsed.canvases["branched-scenario"];

        assert!(canvas.parents.is_empty());
        assert_eq!(canvas.positions["inspect-b"].y, 330.0);
        let graph = reparsed.scenario(&[0]).unwrap().graph.as_ref().unwrap();
        assert_eq!(graph.entries, ["inspect-a", "inspect-b"]);
        assert!(graph.edges.is_empty());
        assert!(!yaml.contains("parents:"));
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
        let yaml = r#"
task:
  id: legacy
  name: Legacy
  description: A legacy standalone scenario.
  platform: macos
  trust: external-allowed
  steps:
    - id: create
      name: Create directory
      type: create-directory
      path: /tmp/ppduster-example
"#;
        let project = load_project_yaml(yaml).unwrap();
        let path = first_scenario_path(&project.entries, &mut Vec::new()).unwrap();

        assert_eq!(project.scenario(&path).unwrap().id, "legacy");
    }

    #[test]
    fn composer_blocks_publish_searchable_output_context_contracts() {
        for kind in ComposerBlockKind::ALL {
            let definition = block_definition(kind.action_kind());
            assert!(!schema_context_lines(&definition.output_schema).is_empty());
        }

        let git = schema_context_lines(&block_definition(ActionKind::GitInspect).output_schema);
        assert!(git
            .iter()
            .any(|line| line == "repository.remote_url : string<git-url>"));
        assert!(git.iter().any(|line| line == "repository.exists : bool"));

        let path = schema_context_lines(&block_definition(ActionKind::InspectPath).output_schema);
        assert!(path
            .iter()
            .any(|line| line == "sha256 : string<sha256> | null (optional)"));
    }

    #[test]
    fn foreach_projection_preserves_only_selected_typed_fields() {
        let mut task = legacy_github_repository_composer_task(1);
        let source = composer_array_sources(&task, 1).remove(0);
        let projected = project_item_type(&source.item_type, &["https_url".into(), "name".into()]);
        let fields = item_object_fields(&projected);

        assert_eq!(
            fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["https_url", "name"]
        );
        assert!(matches!(
            &fields[0].1.value_type,
            ContextType::String {
                format: Some(SemanticFormat::GitUrl)
            }
        ));
        assert!(matches!(
            &fields[1].1.value_type,
            ContextType::String {
                format: Some(SemanticFormat::RepositoryName)
            }
        ));

        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "name".into()],
        };
        task.steps.push(loop_step);
        let lines = composer_step_context_lines(&task, 1);
        assert!(lines
            .iter()
            .any(|line| line == "repository.https_url : string<git-url>"));
        assert!(lines
            .iter()
            .any(|line| { line == "repository.name : string<repository-name>" }));
        assert!(!lines.iter().any(|line| line.contains("repository.ssh_url")));
        assert!(!lines.iter().any(|line| line.starts_with("loop.items[]")));
    }

    #[test]
    fn foreach_array_selector_discovers_typed_upstream_arrays() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        let Action::ForEach {
            source_step,
            array_path,
            item,
            fields,
        } = &mut loop_step.action
        else {
            unreachable!()
        };
        *source_step = "list-repositories".into();
        *array_path = "github.repositories".into();
        *item = "repository".into();
        fields.clear();
        task.steps.push(loop_step);

        assert!(composer_array_sources(&task, 0).is_empty());
        let array_sources = composer_array_sources(&task, 1);
        assert_eq!(array_sources.len(), 1);
        assert_eq!(array_sources[0].step_id, "list-repositories");
        assert_eq!(array_sources[0].step_name, "Получить репозитории аккаунта");
        assert_eq!(array_sources[0].path, "github.repositories");
        assert_eq!(array_sources[0].item, "repository");
        assert!(matches!(
            &array_sources[0].item_type,
            ContextType::Object { .. }
        ));

        let Action::ForEach { fields, .. } = &task.steps[1].action else {
            unreachable!()
        };
        let loop_sources = composer_loop_sources(&task, 2);
        assert_eq!(loop_sources.len(), 1);
        assert_eq!(loop_sources[0].step_id, "loop");
        assert_eq!(loop_sources[0].step_name, "Для каждого элемента");
        assert_eq!(loop_sources[0].source_step, "list-repositories");
        assert_eq!(loop_sources[0].array_path, "github.repositories");
        assert_eq!(loop_sources[0].item, "repository");
        assert_eq!(loop_sources[0].fields, *fields);

        let repository_options = composer_context_options(
            &loop_sources[0],
            &ContextType::string(SemanticFormat::GitUrl),
        );
        assert_eq!(repository_options.len(), 2);
        assert!(repository_options
            .iter()
            .any(|(_, template)| template == "{{repository.https_url}}"));
        assert!(repository_options
            .iter()
            .any(|(_, template)| template == "{{repository.ssh_url}}"));
        assert!(!repository_options
            .iter()
            .any(|(_, template)| template == "{{repository.name}}"));

        let branch_options = composer_context_options(
            &loop_sources[0],
            &ContextType::string(SemanticFormat::GitRef),
        );
        assert_eq!(
            branch_options,
            vec![(
                "repository.default_branch · string<git-ref>".into(),
                "{{repository.default_branch}}".into(),
            )]
        );

        let destination_options = composer_destination_options(
            &loop_sources[0],
            &ContextType::string(SemanticFormat::DirectoryPath),
        );
        assert!(destination_options
            .iter()
            .any(|(_, template)| template == "$HOME/Developer/{{repository.full_name}}"));
        assert!(!destination_options
            .iter()
            .any(|(_, template)| template.contains("https_url")));
    }

    #[test]
    fn foreach_child_repository_input_uses_structural_loop_item_reference() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "ssh_url".into(), "name".into()],
        };
        task.steps.push(loop_step);
        let source = composer_loop_sources(&task, 2).remove(0);
        let inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        let input_schema = definition_for_action(&inspect.action).input_schema;
        let fields = composer_loop_field_options(&source, input_schema.field("repo").unwrap());

        assert_eq!(
            fields
                .iter()
                .map(|field| field.path.as_str())
                .collect::<Vec<_>>(),
            vec!["https_url", "ssh_url"]
        );
        let binding = composer_loop_binding(&source, "https_url");
        assert_eq!(
            binding,
            Binding::field(FieldRef::loop_item("loop").field("https_url"))
        );
        assert!(!matches!(
            &binding,
            Binding::Field { field }
                if field
                    .segments
                    .iter()
                    .any(|segment| matches!(segment, ContextPathSegment::Index { .. }))
        ));
        let selection =
            composer_loop_binding_selection(&binding, std::slice::from_ref(&source)).unwrap();
        assert_eq!(selection.loop_step, "loop");
        assert_eq!(selection.field_path, "https_url");
        assert_eq!(
            composer_loop_binding_preview(&source, &selection, "repo"),
            "repository.https_url → repo"
        );
    }

    #[test]
    fn loop_destination_binding_prefers_full_name_and_falls_back_to_owner_name() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["full_name".into(), "owner".into(), "name".into()],
        };
        task.steps.push(loop_step);
        let source = composer_loop_sources(&task, 2).remove(0);
        let suffixes = composer_loop_destination_suffixes(&source);

        assert_eq!(
            suffixes,
            vec![
                ComposerLoopDestinationSuffix::FullName {
                    field_path: "full_name".into(),
                },
                ComposerLoopDestinationSuffix::OwnerName {
                    owner_path: "owner".into(),
                    name_path: "name".into(),
                },
            ]
        );
        let binding = composer_loop_destination_binding(
            &source,
            "$HOME/Developer",
            suffixes.first().unwrap(),
        );
        assert_eq!(
            binding,
            Binding::interpolated([
                TemplatePart::literal("$HOME/Developer/"),
                TemplatePart::field(FieldRef::loop_item("loop").field("full_name")),
            ])
        );
        let selection = composer_loop_destination_binding_selection(&binding, &source).unwrap();
        assert_eq!(selection.root, "$HOME/Developer");
        assert_eq!(selection.suffix, suffixes[0]);
        assert_eq!(
            composer_loop_destination_preview(&source, &selection),
            "$HOME/Developer/{{repository.full_name}} → dest"
        );
        let empty_root = composer_loop_destination_binding(&source, "", &suffixes[0]);
        assert_eq!(
            composer_loop_destination_binding_selection(&empty_root, &source)
                .unwrap()
                .root,
            ""
        );

        let Action::ForEach { fields, .. } = &mut task.steps[1].action else {
            unreachable!()
        };
        *fields = vec!["owner".into(), "name".into()];
        let fallback = composer_loop_sources(&task, 2).remove(0);
        assert_eq!(
            composer_loop_destination_suffixes(&fallback),
            vec![ComposerLoopDestinationSuffix::OwnerName {
                owner_path: "owner".into(),
                name_path: "name".into(),
            }]
        );
    }

    #[test]
    fn loop_destination_root_validation_is_fail_closed() {
        assert_eq!(composer_destination_root_error("$HOME"), None);
        assert_eq!(composer_destination_root_error("$HOME/Developer"), None);
        assert_eq!(
            composer_destination_root_error("/Users/example/Developer"),
            None
        );
        assert_eq!(
            composer_destination_root_error("Developer"),
            Some("Используйте абсолютный путь либо $HOME/…")
        );
        assert_eq!(
            composer_destination_root_error("$HOME/Developer/../Documents"),
            Some("Базовый каталог не должен содержать '..'.")
        );
        assert_eq!(
            composer_destination_root_error("$HOME/Developer\0escape"),
            Some("Базовый каталог не должен содержать NUL.")
        );
    }

    #[test]
    fn new_git_inspect_child_defaults_to_current_loop_item() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "ssh_url".into(), "full_name".into()],
        };
        task.steps.push(loop_step);
        let mut inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());

        assert!(composer_bind_git_inspect_to_parent_loop(
            &task,
            "loop",
            &mut inspect
        ));
        assert_eq!(
            inspect.bindings.get("repo"),
            Some(&Binding::field(
                FieldRef::loop_item("loop").field("https_url")
            ))
        );
        assert_eq!(
            inspect.bindings.get("dest"),
            Some(&Binding::interpolated([
                TemplatePart::literal("$HOME/Developer/"),
                TemplatePart::field(FieldRef::loop_item("loop").field("full_name")),
            ]))
        );

        task.steps.push(composer_step(
            ComposerBlockKind::InspectPath,
            "intervening".into(),
        ));
        let mut non_child = composer_step(ComposerBlockKind::GitInspect, "non-child".into());
        assert!(!composer_bind_git_inspect_to_parent_loop(
            &task,
            "loop",
            &mut non_child
        ));
        assert!(non_child.bindings.is_empty());
    }

    #[test]
    fn loop_item_repository_binding_round_trips_and_lowers_to_valid_graph() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "ssh_url".into(), "full_name".into()],
        };
        task.steps.push(loop_step);
        let mut inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        assert!(composer_bind_git_inspect_to_parent_loop(
            &task,
            "loop",
            &mut inspect
        ));
        task.steps.push(inspect);

        let task = task.into_v3().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        let decoded = serde_yaml::from_str::<TaskFile>(&yaml).unwrap().task;
        assert!(decoded.steps.is_empty());
        let expected_dest = Binding::interpolated([
            TemplatePart::literal("$HOME/Developer/"),
            TemplatePart::field(FieldRef::loop_item("loop").field("full_name")),
        ]);
        decoded.validate().unwrap();
        let graph = decoded
            .graph
            .as_ref()
            .expect("legacy Task.steps must deserialize into WorkflowGraph v3");
        graph.validate().unwrap();

        let GraphNode::ForEach(loop_node) = &graph.nodes[1] else {
            panic!("expected the immediate consumer to lower into a for-each body")
        };
        let GraphNode::Action(consumer) = &loop_node.body.nodes[0] else {
            panic!("expected GitInspect in the for-each body")
        };
        assert_eq!(
            consumer.bindings.get("repo"),
            Some(&Binding::field(
                FieldRef::loop_item("loop").field("https_url")
            ))
        );
        assert_eq!(consumer.bindings.get("dest"), Some(&expected_dest));
    }

    #[test]
    fn legacy_unbound_loop_child_is_rejected_during_v3_import() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "ssh_url".into()],
        };
        task.steps.push(loop_step);
        task.steps.push(composer_step(
            ComposerBlockKind::GitInspect,
            "inspect".into(),
        ));
        let error = task.into_v3().unwrap_err().to_string();
        assert!(error.contains("cannot be migrated safely") || error.contains("cannot be lowered"));
        assert!(error.contains("does not structurally reference loop item"));
    }

    #[test]
    fn indexed_loop_child_requires_explicit_migration_before_validation() {
        let mut task = legacy_github_repository_composer_task(1);
        let array_source = composer_array_sources(&task, 1).remove(0);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "ssh_url".into()],
        };
        task.steps.push(loop_step);
        let mut inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        let indexed = composer_indexed_binding(&array_source, 2, "https_url");
        inspect.bindings.insert("repo".into(), indexed.clone());
        task.steps.push(inspect);
        let canvas = ComposerCanvas {
            parents: BTreeMap::from([
                ("list-repositories".into(), "start".into()),
                ("loop".into(), "list-repositories".into()),
                ("inspect".into(), "loop".into()),
            ]),
            ..ComposerCanvas::default()
        };
        let mut project = composer_project_with_canvas(task, canvas);

        assert_eq!(
            project.scenario(&[0]).unwrap().steps[2]
                .bindings
                .get("repo"),
            Some(&indexed)
        );
        assert!(validate_project_structure(&project).is_err());
        assert!(validate_project_for_editing(&project).is_err());
        assert!(validate_project(&project).is_err());

        let task = project.scenario_mut(&[0]).unwrap();
        task.steps[2].bindings.insert(
            "repo".into(),
            Binding::field(FieldRef::loop_item("loop").field("https_url")),
        );
        validate_project(&project).unwrap();
    }

    #[test]
    fn scoped_discovery_hides_loop_and_body_outputs_from_downstream() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        };
        task.steps.push(loop_step);
        task.steps.push(composer_step(
            ComposerBlockKind::GithubListRepositories,
            "body-output".into(),
        ));
        task.steps.push(composer_step(
            ComposerBlockKind::InspectPath,
            "downstream".into(),
        ));
        let canvas = ComposerCanvas {
            parents: BTreeMap::from([
                ("list-repositories".into(), "start".into()),
                ("loop".into(), "list-repositories".into()),
                ("body-output".into(), "loop".into()),
                ("downstream".into(), "body-output".into()),
            ]),
            ..ComposerCanvas::default()
        };

        assert!(composer_array_sources(&task, 4)
            .iter()
            .any(|source| source.step_id == "body-output"));
        assert!(!composer_array_sources_scoped(&task, 4, Some(&canvas))
            .iter()
            .any(|source| source.step_id == "body-output"));
        assert!(composer_condition_fields_scoped(&task, 4, None)
            .iter()
            .any(|field| matches!(
                &field.reference.scope,
                ContextScope::Step { step_id } if step_id == "body-output"
            )));
        assert!(!composer_condition_fields_scoped(&task, 4, Some(&canvas))
            .iter()
            .any(|field| matches!(
                &field.reference.scope,
                ContextScope::Step { step_id } if step_id == "body-output"
            )));
        assert!(!composer_step_context_lines(&task, 1)
            .iter()
            .any(|line| line.starts_with("loop.")));
    }

    #[test]
    fn non_immediate_loop_children_are_not_connectable_or_valid() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        };
        task.steps.push(loop_step);
        assert!(composer_parent_accepts_new_child(&task, "loop"));

        task.steps.push(composer_step(
            ComposerBlockKind::InspectPath,
            "intervening".into(),
        ));
        let mut inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(FieldRef::loop_item("loop").field("https_url")),
        );
        task.steps.push(inspect);
        let canvas = ComposerCanvas {
            parents: BTreeMap::from([
                ("list-repositories".into(), "start".into()),
                ("loop".into(), "list-repositories".into()),
                ("intervening".into(), "start".into()),
                ("inspect".into(), "loop".into()),
            ]),
            ..ComposerCanvas::default()
        };

        assert!(!composer_parent_accepts_new_child(&task, "loop"));
        assert!(!composer_canvas_edge_is_visible(&task, "inspect", "loop"));
        assert!(validate_composer_canvas(&task, &canvas)
            .unwrap_err()
            .contains("не идёт сразу после"));
    }

    #[test]
    fn canvas_validation_rejects_stale_edges_and_unbound_loops() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        };
        task.steps.push(loop_step);

        let stale_child = ComposerCanvas {
            parents: BTreeMap::from([("ghost".into(), "start".into())]),
            ..ComposerCanvas::default()
        };
        assert!(validate_composer_canvas(&task, &stale_child)
            .unwrap_err()
            .contains("неизвестный дочерний блок"));

        let stale_parent = ComposerCanvas {
            parents: BTreeMap::from([("list-repositories".into(), "ghost".into())]),
            ..ComposerCanvas::default()
        };
        assert!(validate_composer_canvas(&task, &stale_parent)
            .unwrap_err()
            .contains("неизвестный родительский блок"));

        let no_child = ComposerCanvas {
            parents: BTreeMap::from([
                ("list-repositories".into(), "start".into()),
                ("loop".into(), "list-repositories".into()),
            ]),
            ..ComposerCanvas::default()
        };
        assert!(validate_composer_canvas(&task, &no_child)
            .unwrap_err()
            .contains("ровно один непосредственный дочерний блок"));
    }

    #[test]
    fn loop_item_ui_is_available_only_for_immediate_canvas_child() {
        let mut task = legacy_github_repository_composer_task(1);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        };
        task.steps.push(loop_step);
        task.steps.push(composer_step(
            ComposerBlockKind::GitInspect,
            "inspect".into(),
        ));
        let direct_canvas = ComposerCanvas {
            parents: BTreeMap::from([("inspect".into(), "loop".into())]),
            ..ComposerCanvas::default()
        };
        assert_eq!(
            composer_parent_loop_id(&task, 2, Some(&direct_canvas)).as_deref(),
            Some("loop")
        );

        task.steps.insert(
            2,
            composer_step(ComposerBlockKind::InspectPath, "intervening".into()),
        );
        assert_eq!(
            composer_parent_loop_id(&task, 3, Some(&direct_canvas)),
            None
        );

        let sibling_canvas = ComposerCanvas {
            parents: BTreeMap::from([("inspect".into(), "start".into())]),
            ..ComposerCanvas::default()
        };
        task.steps.remove(2);
        assert_eq!(
            composer_parent_loop_id(&task, 2, Some(&sibling_canvas)),
            None
        );
    }

    #[test]
    fn legacy_indexed_binding_is_only_offered_as_an_explicit_loop_migration() {
        let mut task = legacy_github_repository_composer_task(1);
        let array_source = composer_array_sources(&task, 1).remove(0);
        let mut loop_step = composer_step(ComposerBlockKind::ForEach, "loop".into());
        loop_step.action = Action::ForEach {
            source_step: "list-repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: Vec::new(),
        };
        task.steps.push(loop_step);
        let loop_source = composer_loop_sources(&task, 2).remove(0);
        let original = composer_indexed_binding(&array_source, 2, "https_url");

        assert_eq!(
            composer_indexed_binding_for_loop(
                &original,
                &loop_source,
                std::slice::from_ref(&array_source),
            ),
            Some(ComposerLoopBinding {
                loop_step: "loop".into(),
                field_path: "https_url".into(),
            })
        );
        assert_eq!(
            original,
            composer_indexed_binding(&array_source, 2, "https_url"),
            "detecting the migration must not rewrite persisted semantics"
        );
    }

    #[test]
    fn indexed_repository_input_maps_third_item_to_zero_based_field_ref() {
        let task = legacy_github_repository_composer_task(1);
        let source = composer_array_sources(&task, 1).remove(0);
        let inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        let input_schema = definition_for_action(&inspect.action).input_schema;
        let expected = input_schema.field("repo").unwrap();
        let fields = composer_indexed_field_options(&source, expected);

        assert_eq!(
            fields
                .iter()
                .map(|field| field.path.as_str())
                .collect::<Vec<_>>(),
            vec!["https_url", "ssh_url"]
        );

        let binding = composer_indexed_binding(&source, 2, "https_url");
        assert_eq!(
            binding,
            Binding::field(
                FieldRef::step("list-repositories")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("https_url")
            )
        );
        let selection =
            composer_indexed_binding_selection(&binding, std::slice::from_ref(&source)).unwrap();
        assert_eq!(selection.index, 2);
        assert_eq!(selection.field_path, "https_url");
        assert_eq!(
            composer_indexed_binding_preview(&selection, "repo"),
            "list-repositories.github.repositories[3].https_url → repo"
        );
    }

    #[test]
    fn indexed_input_picker_filters_fields_by_input_contract() {
        let task = legacy_github_repository_composer_task(1);
        let source = composer_array_sources(&task, 1).remove(0);
        let inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        let input_schema = definition_for_action(&inspect.action).input_schema;

        let repository_fields =
            composer_indexed_field_options(&source, input_schema.field("repo").unwrap());
        assert_eq!(repository_fields.len(), 2);
        assert!(repository_fields.iter().all(|field| matches!(
            field.value_type,
            ContextType::String {
                format: Some(SemanticFormat::GitUrl)
            }
        )));
        assert!(
            composer_indexed_field_options(&source, input_schema.field("dest").unwrap()).is_empty()
        );
    }

    #[test]
    fn indexed_input_picker_rejects_optional_nullable_and_secret_fields() {
        let git_url = ContextType::string(SemanticFormat::GitUrl);
        let item_type = ContextType::object(
            ObjectSchema::new("test.repository@1")
                .with_field("required", FieldSchema::required(git_url.clone()))
                .with_field("optional", FieldSchema::optional(git_url.clone()))
                .with_field(
                    "nullable",
                    FieldSchema::required(git_url.clone()).nullable(),
                )
                .with_field(
                    "secret",
                    FieldSchema::required(git_url.clone()).sensitive(Sensitivity::Secret),
                ),
        );
        let source = ComposerArraySource {
            step_id: "source".into(),
            step_name: "Source".into(),
            path: "repositories".into(),
            item: "repository".into(),
            item_type,
        };
        let expected = FieldSchema::required(git_url);

        assert_eq!(
            composer_indexed_field_options(&source, &expected)
                .into_iter()
                .map(|field| field.path)
                .collect::<Vec<_>>(),
            vec!["required"]
        );
    }

    #[test]
    fn step_binding_serde_preserves_indexed_repository_selection() {
        let mut task = legacy_github_repository_composer_task(1);
        let source = composer_array_sources(&task, 1).remove(0);
        let mut inspect = composer_step(ComposerBlockKind::GitInspect, "inspect".into());
        inspect.bindings.insert(
            "repo".into(),
            composer_indexed_binding(&source, 2, "ssh_url"),
        );
        let expected = inspect.bindings["repo"].clone();
        task.steps.push(inspect);

        let task = task.into_v3().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        let decoded = serde_yaml::from_str::<TaskFile>(&yaml).unwrap().task;
        assert!(decoded.steps.is_empty());
        let GraphNode::Action(inspect) = &decoded.graph.as_ref().unwrap().nodes[1] else {
            panic!("expected imported inspect action")
        };
        assert_eq!(inspect.bindings.get("repo"), Some(&expected));
        decoded.validate().unwrap();
    }

    #[test]
    fn array_selector_recursively_discovers_non_github_arrays() {
        let mut task = legacy_github_repository_composer_task(1);
        task.steps[0] = Step {
            id: "run-script".into(),
            name: "Run a script".into(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: true,
            allow_elevation: Default::default(),
            when: None,
            require: None,
            action: Action::RunScript {
                interpreter: ScriptInterpreter::Sh,
                script: "script.sh".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                success_exit_codes: vec![0, 2],
            },
        };

        let sources = composer_array_sources(&task, 1);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].step_id, "run-script");
        assert_eq!(sources[0].path, "success_exit_codes");
        assert_eq!(sources[0].item, "success_exit_code");
        assert_eq!(sources[0].item_type, ContextType::Integer);
    }

    #[test]
    fn condition_picker_exposes_only_previous_step_schemas() {
        let mut task = legacy_github_repository_composer_task(1);
        task.steps.push(composer_step(
            ComposerBlockKind::InspectPath,
            "inspect-path".into(),
        ));
        task.steps.push(composer_step(
            ComposerBlockKind::GitInspect,
            "future-git".into(),
        ));

        assert!(composer_condition_fields_scoped(&task, 0, None).is_empty());
        let before_inspect = composer_condition_fields_scoped(&task, 1, None);
        assert!(!before_inspect.is_empty());
        assert!(before_inspect.iter().all(|field| {
            matches!(
                &field.reference.scope,
                ContextScope::Step { step_id } if step_id == "list-repositories"
            )
        }));
        assert!(before_inspect.iter().any(|field| {
            field_ref_label(&field.reference) == "list-repositories.github.account.login"
        }));

        let before_future = composer_condition_fields_scoped(&task, 2, None);
        assert!(before_future.iter().any(|field| {
            field_ref_label(&field.reference) == "inspect-path.exists"
                && field.value_type == ContextType::Boolean
        }));
        assert!(!before_future
            .iter()
            .any(|field| field_ref_label(&field.reference).starts_with("future-git.")));
    }

    #[test]
    fn condition_operators_and_literals_follow_field_types() {
        use ComposerConditionOperator as Operator;
        use ComposerLiteralKind as Literal;

        let string_operators = condition_operators(&ContextType::string(SemanticFormat::GitUrl));
        assert!(string_operators.contains(&Operator::Equal));
        assert!(string_operators.contains(&Operator::Contains));
        assert!(string_operators.contains(&Operator::StartsWith));
        assert!(string_operators.contains(&Operator::EndsWith));
        assert!(string_operators.contains(&Operator::Matches));
        assert!(string_operators.contains(&Operator::IsEmpty));
        assert!(!string_operators.contains(&Operator::GreaterThan));

        let numeric_operators = condition_operators(&ContextType::Integer);
        assert!(numeric_operators.contains(&Operator::LessThan));
        assert!(numeric_operators.contains(&Operator::GreaterThanOrEqual));
        assert!(!numeric_operators.contains(&Operator::Contains));

        let boolean_operators = condition_operators(&ContextType::Boolean);
        assert_eq!(
            boolean_operators,
            vec![
                Operator::Equal,
                Operator::NotEqual,
                Operator::Exists,
                Operator::IsNull,
            ]
        );
        let object_operators = condition_operators(&ContextType::object(ObjectSchema::new(
            "test.condition.object@1",
        )));
        assert_eq!(
            object_operators,
            vec![Operator::IsEmpty, Operator::Exists, Operator::IsNull]
        );

        let nullable_branch = ComposerConditionField {
            reference: FieldRef::step("list").field("default_branch"),
            label: String::new(),
            value_type: ContextType::string(SemanticFormat::GitRef),
            required: false,
            nullable: true,
        };
        assert_eq!(
            condition_literal_kinds(&nullable_branch, Operator::Equal),
            vec![Literal::String, Literal::Null]
        );
        assert!(matches!(
            default_condition_literal(&nullable_branch, Operator::Equal),
            Some(ExpressionValue::String(value)) if value.is_empty()
        ));
        let nullable_number = ComposerConditionField {
            value_type: ContextType::Number,
            ..nullable_branch
        };
        assert_eq!(
            condition_literal_kinds(&nullable_number, Operator::GreaterThan),
            vec![Literal::Number]
        );
    }

    #[test]
    fn typed_when_and_require_conditions_round_trip_through_yaml() {
        let field = FieldRef::step("list-repositories")
            .field("github")
            .field("account")
            .field("login");
        let when_rule = SimpleConditionRule {
            field: field.clone(),
            operator: ComposerConditionOperator::StartsWith,
            literal: Some(ExpressionValue::String("octo".into())),
        };
        let require_rule = SimpleConditionRule {
            field,
            operator: ComposerConditionOperator::Exists,
            literal: None,
        };
        let mut step = composer_step(ComposerBlockKind::InspectPath, "inspect".into());
        step.when = Some(StepCondition::Expression {
            rule: build_simple_condition_rule(&when_rule),
            policy: RuleOutcomePolicy {
                on_null: IndeterminatePolicy::TreatAsFalse,
                on_missing: IndeterminatePolicy::TreatAsTrue,
                on_unknown: IndeterminatePolicy::Fail,
            },
        });
        step.require = Some(StepCondition::Expression {
            rule: build_simple_condition_rule(&require_rule),
            policy: RuleOutcomePolicy::default(),
        });

        let mut task = legacy_github_repository_composer_task(1);
        task.steps.push(step.clone());
        task.validate().unwrap();

        let yaml = serde_yaml::to_string(&step).unwrap();
        let decoded: Step = serde_yaml::from_str(&yaml).unwrap();
        let Some(StepCondition::Expression { rule, policy }) = decoded.when else {
            panic!("typed when condition was not preserved")
        };
        assert_eq!(simple_condition_rule(&rule), Some(when_rule));
        assert_eq!(policy.on_null, IndeterminatePolicy::TreatAsFalse);
        assert_eq!(policy.on_missing, IndeterminatePolicy::TreatAsTrue);
        assert_eq!(policy.on_unknown, IndeterminatePolicy::Fail);
        let Some(StepCondition::Expression { rule, .. }) = decoded.require else {
            panic!("typed require condition was not preserved")
        };
        assert_eq!(simple_condition_rule(&rule), Some(require_rule));
    }

    #[test]
    fn regex_and_empty_rules_round_trip_through_the_visual_model() {
        let field = FieldRef::step("list-repositories")
            .field("github")
            .field("account")
            .field("login");
        let regex = SimpleConditionRule {
            field: field.clone(),
            operator: ComposerConditionOperator::Matches,
            literal: Some(ExpressionValue::String("^[a-z0-9-]+$".into())),
        };
        let empty = SimpleConditionRule {
            field,
            operator: ComposerConditionOperator::IsEmpty,
            literal: None,
        };

        assert_eq!(
            simple_condition_rule(&build_simple_condition_rule(&regex)),
            Some(regex.clone())
        );
        assert_eq!(
            simple_condition_rule(&build_simple_condition_rule(&empty)),
            Some(empty.clone())
        );

        let grouped = ComposerConditionRule::All(vec![
            ComposerConditionRule::Clause(regex),
            ComposerConditionRule::Not(Box::new(ComposerConditionRule::Any(vec![
                ComposerConditionRule::Clause(empty.clone()),
                ComposerConditionRule::Clause(empty),
            ]))),
        ]);
        let expression = build_composer_condition_rule(&grouped);
        assert_eq!(composer_condition_rule(&expression), Some(grouped));
    }

    #[test]
    fn nested_visual_rule_and_policy_round_trip_without_loss() {
        let field = FieldRef::step("list-repositories")
            .field("github")
            .field("account")
            .field("login");
        let clause = |operator, literal| {
            ComposerConditionRule::Clause(SimpleConditionRule {
                field: field.clone(),
                operator,
                literal,
            })
        };
        let editable = ComposerConditionRule::Any(vec![
            clause(
                ComposerConditionOperator::Matches,
                Some(ExpressionValue::String("^(octo|hubot)$".into())),
            ),
            ComposerConditionRule::Not(Box::new(ComposerConditionRule::All(vec![
                clause(ComposerConditionOperator::IsEmpty, None),
                clause(
                    ComposerConditionOperator::NotEqual,
                    Some(ExpressionValue::String("archived".into())),
                ),
            ]))),
        ]);
        let policy = RuleOutcomePolicy {
            on_null: IndeterminatePolicy::TreatAsFalse,
            on_missing: IndeterminatePolicy::TreatAsTrue,
            on_unknown: IndeterminatePolicy::Fail,
        };
        let condition = StepCondition::Expression {
            rule: build_composer_condition_rule(&editable),
            policy,
        };

        let yaml = serde_yaml::to_string(&condition).unwrap();
        let decoded: StepCondition = serde_yaml::from_str(&yaml).unwrap();
        let StepCondition::Expression { rule, policy: got } = decoded else {
            panic!("typed expression changed variants")
        };
        assert_eq!(got, policy);
        let reparsed = composer_condition_rule(&rule).expect("rule remains visually editable");
        assert_eq!(reparsed, editable);
        assert_eq!(build_composer_condition_rule(&reparsed), rule);
    }

    #[test]
    fn unsupported_quantifier_remains_read_only_and_serializes_unchanged() {
        let rule = ExpressionV1::Quantifier {
            quantifier: ppduster::automation::CollectionQuantifier::Any,
            collection: Box::new(ExpressionV1::Ref {
                reference: ReferenceV1::Context {
                    field: FieldRef::step("list").field("github").field("repositories"),
                },
            }),
            binding: "repository".into(),
            predicate: Box::new(ExpressionV1::Matches {
                value: Box::new(ExpressionV1::Ref {
                    reference: ReferenceV1::Local {
                        binding: "repository".into(),
                        path: vec!["name".into()],
                    },
                }),
                pattern: "^ppduster$".into(),
            }),
        };
        let condition = StepCondition::Expression {
            rule: rule.clone(),
            policy: RuleOutcomePolicy {
                on_null: IndeterminatePolicy::TreatAsFalse,
                on_missing: IndeterminatePolicy::Fail,
                on_unknown: IndeterminatePolicy::TreatAsTrue,
            },
        };

        assert!(composer_condition_rule(&rule).is_none());
        let yaml = serde_yaml::to_string(&condition).unwrap();
        let decoded: StepCondition = serde_yaml::from_str(&yaml).unwrap();
        let StepCondition::Expression {
            rule: decoded_rule,
            policy,
        } = decoded
        else {
            panic!("quantifier expression changed variants")
        };
        assert_eq!(decoded_rule, rule);
        assert_eq!(policy.on_null, IndeterminatePolicy::TreatAsFalse);
        assert_eq!(policy.on_missing, IndeterminatePolicy::Fail);
        assert_eq!(policy.on_unknown, IndeterminatePolicy::TreatAsTrue);
    }

    #[test]
    fn visual_rule_parser_enforces_depth_and_node_budgets() {
        let clause = || ExpressionV1::Exists {
            reference: ReferenceV1::Context {
                field: FieldRef::step("inspect").field("exists"),
            },
        };
        let at_node_limit = ExpressionV1::All {
            expressions: (0..CONDITION_EDITOR_MAX_NODES - 1)
                .map(|_| clause())
                .collect(),
        };
        assert!(composer_condition_rule(&at_node_limit).is_some());
        let over_node_limit = ExpressionV1::All {
            expressions: (0..CONDITION_EDITOR_MAX_NODES).map(|_| clause()).collect(),
        };
        assert!(composer_condition_rule(&over_node_limit).is_none());

        let nested = |count| {
            (0..count).fold(clause(), |expression, _| ExpressionV1::Not {
                expression: Box::new(expression),
            })
        };
        assert!(composer_condition_rule(&nested(CONDITION_EDITOR_MAX_DEPTH)).is_some());
        assert!(composer_condition_rule(&nested(CONDITION_EDITOR_MAX_DEPTH + 1)).is_none());

        let current = composer_condition_rule(&clause()).unwrap();
        let negated = ComposerConditionRule::Not(Box::new(current.clone()));
        assert!(!composer_condition_replacement_fits(
            &current,
            &negated,
            0,
            CONDITION_EDITOR_MAX_NODES,
        ));
        assert!(!composer_condition_replacement_fits(
            &current,
            &negated,
            CONDITION_EDITOR_MAX_DEPTH,
            1,
        ));
        let grouped = ComposerConditionRule::All(vec![current.clone(), current.clone()]);
        assert!(composer_condition_replacement_fits(
            &current, &grouped, 0, 1,
        ));
    }

    #[test]
    fn regex_feedback_accepts_unicode_and_rejects_invalid_or_oversized_patterns() {
        assert!(regex_pattern_error("^(привет|мир)\\s+🚀$").is_none());
        assert!(regex_pattern_error("(")
            .expect("unclosed group must be rejected")
            .contains("Некорректное"));
        let oversized = "я".repeat(ExpressionLimits::default().max_regex_pattern_bytes);
        assert!(regex_pattern_error(&oversized)
            .expect("byte limit must be enforced for unicode too")
            .contains("максимум"));
    }

    #[test]
    fn github_composer_scenario_publishes_repository_array_contract() {
        let task = github_repository_composer_task(3);

        assert_eq!(task.id, "github-repositories-3");
        assert_eq!(task.name, "Получить репозитории GitHub");
        assert!(task.steps.is_empty());
        let graph = task.graph.as_ref().expect("graph-native composer task");
        assert_eq!(graph.entries, ["list-repositories"]);
        let GraphNode::Action(action) = &graph.nodes[0] else {
            panic!("expected GitHub repository action node")
        };
        assert!(matches!(action.step.action, Action::GithubListRepositories));
        let lines = schema_context_lines(&definition_for_action(&action.step.action).output_schema);
        assert!(lines
            .iter()
            .any(|line| line == "github.account.login : string<identifier>"));
        assert!(lines
            .iter()
            .any(|line| line == "github.repositories[] : object"));
        assert!(lines
            .iter()
            .any(|line| { line == "github.repositories[].https_url : string<git-url>" }));
        assert!(lines.iter().any(|line| {
            line == "github.repositories[].default_branch : string<git-ref> | null (optional)"
        }));
        assert!(lines
            .iter()
            .all(|line| !line.contains(',') && line.len() < 96));
        assert!(matches!(action.step.auth, AuthPolicy::None));
        task.validate().unwrap();
        let yaml = serde_yaml::to_string(&TaskFile { task }).unwrap();
        assert!(yaml.contains("type: github-list-repositories"));
        assert!(yaml.contains("format_version: 3"));
        assert!(yaml.contains("workflow_graph:"));
        assert!(!yaml.contains("\n  steps:"));
        let round_trip: TaskFile = serde_yaml::from_str(&yaml).unwrap();
        assert!(round_trip.task.steps.is_empty());
        let GraphNode::Action(round_trip_action) =
            &round_trip.task.graph.as_ref().unwrap().nodes[0]
        else {
            panic!("expected round-tripped GitHub action node")
        };
        assert!(matches!(
            round_trip_action.step.action,
            Action::GithubListRepositories
        ));
    }

    #[test]
    fn unsupported_if_preview_is_byte_stable_until_explicit_replacement() {
        let condition = ExpressionV1::In {
            needle: Box::new(ExpressionV1::Literal {
                value: ExpressionValue::String("main".into()),
            }),
            collection: Box::new(ExpressionV1::Literal {
                value: ExpressionValue::List(vec![ExpressionValue::String("main".into())]),
            }),
        };
        let before = serde_yaml::to_string(&condition).unwrap();

        assert!(composer_condition_rule(&condition).is_none());
        assert!(condition_read_only_summary(&condition).contains("in"));
        assert_eq!(serde_yaml::to_string(&condition).unwrap(), before);
        assert!(matches!(
            default_graph_if_condition(&[]),
            ExpressionV1::Literal {
                value: ExpressionValue::Bool(true)
            }
        ));
    }

    #[test]
    fn graph_delete_never_cascades_or_breaks_structural_references() {
        let action = |id: &str| {
            GraphNode::Action(Box::new(ActionNode {
                step: default_step(ActionKind::GitInspect, id).unwrap(),
                bindings: BTreeMap::new(),
            }))
        };
        let mut chain = WorkflowGraph {
            entries: vec!["a".into()],
            nodes: vec![action("a"), action("b"), action("c")],
            edges: vec![
                GraphEdge::new("a", EdgePort::Success, "b"),
                GraphEdge::new("b", EdgePort::Success, "c"),
            ],
            ..WorkflowGraph::default()
        };
        let before = serde_yaml::to_string(&chain).unwrap();
        let error = graph_remove_composer_node(&mut chain, "b").unwrap_err();
        assert!(error.contains("downstream"));
        assert_eq!(serde_yaml::to_string(&chain).unwrap(), before);
        assert!(graph_remove_composer_node(&mut chain, "c").unwrap());
        assert!(graph_node(&chain, "b").is_some());

        let mut referenced = WorkflowGraph {
            entries: vec!["producer".into(), "alternate".into()],
            nodes: vec![
                action("producer"),
                action("alternate"),
                GraphNode::Action(Box::new(ActionNode {
                    step: default_step(ActionKind::GitInspect, "consumer").unwrap(),
                    bindings: BTreeMap::from([(
                        "repo".into(),
                        Binding::field(FieldRef::step("producer").field("repository")),
                    )]),
                })),
            ],
            edges: vec![
                GraphEdge::new("producer", EdgePort::Success, "consumer"),
                GraphEdge::new("alternate", EdgePort::Success, "consumer"),
            ],
            ..WorkflowGraph::default()
        };
        let error = graph_remove_composer_node(&mut referenced, "producer").unwrap_err();
        assert!(error.contains("привязки и условия"));
        assert!(graph_node(&referenced, "producer").is_some());

        let mut control = WorkflowGraph {
            entries: vec!["loop".into()],
            nodes: vec![GraphNode::ForEach(ForEachNode {
                id: "loop".into(),
                collection: Binding::literal(serde_json::json!([])),
                item_alias: "item".into(),
                index_alias: None,
                concurrency: 1,
                on_error: LoopFailurePolicy::Stop,
                body: Box::new(WorkflowGraph {
                    entries: vec!["child".into()],
                    nodes: vec![action("child")],
                    ..WorkflowGraph::default()
                }),
            })],
            ..WorkflowGraph::default()
        };
        let error = graph_remove_composer_node(&mut control, "loop").unwrap_err();
        assert!(error.contains("вложенных ветвей"));
        assert!(graph_node(&control, "child").is_some());
    }

    #[test]
    fn graph_attach_uses_explicit_source_port_and_rejects_invalid_port_atomically() {
        let mut graph = WorkflowGraph {
            entries: vec!["source".into()],
            nodes: vec![GraphNode::Action(Box::new(ActionNode {
                step: default_step(ActionKind::GitInspect, "source").unwrap(),
                bindings: BTreeMap::new(),
            }))],
            ..WorkflowGraph::default()
        };
        let attach = ComposerGraphAttach::RootAfter {
            node_id: "source".into(),
        };
        let child = graph_insert_composer_block(
            &mut graph,
            &attach,
            Some(EdgePort::Failure),
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        assert!(graph.edges.iter().any(|edge| {
            edge.from.node == "source"
                && edge.from.port == EdgePort::Failure
                && edge.to.node == child
        }));

        let node_count = graph.nodes.len();
        let error = graph_insert_composer_block(
            &mut graph,
            &attach,
            Some(EdgePort::Empty),
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap_err();
        assert!(error.contains("недоступен"));
        assert_eq!(graph.nodes.len(), node_count);
    }

    fn graph_authoring_test_task(id: &str, graph: WorkflowGraph) -> Task {
        Task {
            id: id.into(),
            name: id.into(),
            description: "generated graph authoring test".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            graph: Some(graph),
            steps: Vec::new(),
        }
    }

    fn assert_graph_authoring_round_trip(id: &str, graph: &WorkflowGraph) {
        graph.validate().unwrap_or_else(|errors| {
            panic!(
                "generated graph {id} must validate: {}",
                errors
                    .into_iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        });
        let yaml = serde_yaml::to_string(&TaskFile {
            task: graph_authoring_test_task(id, graph.clone()),
        })
        .unwrap();
        let decoded: TaskFile = serde_yaml::from_str(&yaml).unwrap();
        assert!(decoded.task.steps.is_empty());
        assert!(decoded.task.is_v3());
        decoded.task.validate().unwrap_or_else(|error| {
            panic!("round-tripped generated graph {id} must validate: {error}")
        });
        assert_eq!(
            serde_yaml::to_string(decoded.task.graph.as_ref().unwrap()).unwrap(),
            serde_yaml::to_string(graph).unwrap(),
            "graph authoring round trip changed {id}"
        );
    }

    #[test]
    fn graph_authoring_matrix_covers_every_action_at_root_and_in_foreach() {
        let action_kinds = ActionKind::ALL
            .into_iter()
            .filter(|kind| kind.is_graph_action())
            .collect::<Vec<_>>();
        assert_eq!(
            action_kinds.len(),
            23,
            "update the authoring matrix for new actions"
        );

        for kind in action_kinds {
            let mut root = WorkflowGraph::default();
            let root_id = graph_insert_composer_block(
                &mut root,
                &ComposerGraphAttach::RootStart,
                None,
                ComposerGraphBlockKind::Action(kind),
            )
            .unwrap_or_else(|error| panic!("root authoring failed for {}: {error}", kind.id()));
            let GraphNode::Action(root_action) = graph_node(&root, &root_id).unwrap() else {
                panic!("{} was not authored as an action", kind.id())
            };
            assert_eq!(root_action.step.action.kind(), kind);
            assert_graph_authoring_round_trip(&format!("root-{}", kind.id()), &root);

            let mut nested = WorkflowGraph::default();
            let loop_id = graph_insert_composer_block(
                &mut nested,
                &ComposerGraphAttach::RootStart,
                None,
                ComposerGraphBlockKind::ForEach,
            )
            .unwrap();
            let nested_id = graph_insert_composer_block(
                &mut nested,
                &ComposerGraphAttach::NestedStart {
                    scope: ComposerGraphNestedScope::ForEachBody {
                        owner_id: loop_id.clone(),
                    },
                },
                None,
                ComposerGraphBlockKind::Action(kind),
            )
            .unwrap_or_else(|error| panic!("nested authoring failed for {}: {error}", kind.id()));
            let GraphNode::Action(nested_action) = graph_node(&nested, &nested_id).unwrap() else {
                panic!("nested {} was not authored as an action", kind.id())
            };
            assert_eq!(nested_action.step.action.kind(), kind);
            assert_graph_authoring_round_trip(&format!("nested-{}", kind.id()), &nested);
        }
    }

    #[test]
    fn repository_loop_autobinding_is_semantic_unambiguous_and_recipe_scoped() {
        let mut graph = WorkflowGraph::default();
        let list_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Action(ActionKind::GithubListRepositories),
        )
        .unwrap();
        let loop_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootAfter { node_id: list_id },
            Some(EdgePort::Success),
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();

        let repository_item = graph_loop_item_type(&graph, &loop_id).unwrap();
        let run_definition = block_definition(ActionKind::RunCommand);
        assert!(unique_exact_loop_binding_field(
            &repository_item,
            run_definition.input_schema.field("program").unwrap()
        )
        .is_none());
        let inspect_definition = block_definition(ActionKind::InspectPath);
        assert!(unique_exact_loop_binding_field(
            &repository_item,
            inspect_definition
                .input_schema
                .field("recursive_size")
                .unwrap()
        )
        .is_none());
        let clone_definition = block_definition(ActionKind::GitClone);
        assert_eq!(
            unique_exact_loop_binding_field(
                &repository_item,
                clone_definition.input_schema.field("branch").unwrap()
            )
            .map(|field| field.path),
            Some("default_branch".into())
        );

        let one_plain_string =
            ContextType::object(ObjectSchema::new("test.loop-item@1").with_field(
                "label",
                FieldSchema::required(ContextType::String { format: None }),
            ));
        assert_eq!(
            unique_exact_loop_binding_field(
                &one_plain_string,
                &FieldSchema::required(ContextType::String { format: None })
            )
            .map(|field| field.path),
            Some("label".into())
        );

        for kind in [
            ActionKind::RunCommand,
            ActionKind::WriteFile,
            ActionKind::InspectPath,
        ] {
            let action_id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::NestedStart {
                    scope: ComposerGraphNestedScope::ForEachBody {
                        owner_id: loop_id.clone(),
                    },
                },
                None,
                ComposerGraphBlockKind::Action(kind),
            )
            .unwrap();
            let GraphNode::Action(action) = graph_node(&graph, &action_id).unwrap() else {
                panic!("expected action node")
            };
            assert!(
                action.bindings.is_empty(),
                "{} received unrelated repository fields: {:?}",
                kind.id(),
                action.bindings
            );
        }

        let clone_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: loop_id.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::GitClone),
        )
        .unwrap();
        let GraphNode::Action(clone) = graph_node(&graph, &clone_id).unwrap() else {
            panic!("expected Git clone action node")
        };
        assert_eq!(
            clone.bindings.get("repo"),
            Some(&Binding::field(
                FieldRef::loop_item(&loop_id).field("https_url")
            ))
        );
        assert_eq!(
            clone.bindings.get("branch"),
            Some(&Binding::field(
                FieldRef::loop_item(&loop_id).field("default_branch")
            ))
        );
        assert!(matches!(
            clone.bindings.get("dest"),
            Some(Binding::Interpolated { .. })
        ));
        assert_graph_authoring_round_trip("repository-loop-semantic-bindings", &graph);
    }

    #[test]
    fn github_loop_materializes_all_git_recipes_without_nullable_required_branch_flow() {
        for kind in [
            ActionKind::GitClone,
            ActionKind::GitInspect,
            ActionKind::GitCloneIfMissing,
            ActionKind::GitFetch,
            ActionKind::GitFastForward,
        ] {
            let mut graph = WorkflowGraph::default();
            let list_id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootStart,
                None,
                ComposerGraphBlockKind::Action(ActionKind::GithubListRepositories),
            )
            .unwrap();
            let loop_id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootAfter { node_id: list_id },
                Some(EdgePort::Success),
                ComposerGraphBlockKind::ForEach,
            )
            .unwrap();
            let action_id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::NestedStart {
                    scope: ComposerGraphNestedScope::ForEachBody {
                        owner_id: loop_id.clone(),
                    },
                },
                None,
                ComposerGraphBlockKind::Action(kind),
            )
            .unwrap();
            let GraphNode::Action(action) = graph_node(&graph, &action_id).unwrap() else {
                panic!("Git action expected")
            };
            assert_eq!(
                action.bindings.get("repo"),
                Some(&Binding::field(
                    FieldRef::loop_item(&loop_id).field("https_url")
                ))
            );
            assert!(matches!(
                action.bindings.get("dest"),
                Some(Binding::Interpolated { .. })
            ));
            if matches!(kind, ActionKind::GitClone | ActionKind::GitCloneIfMissing) {
                assert_eq!(
                    action.bindings.get("branch"),
                    Some(&Binding::field(
                        FieldRef::loop_item(&loop_id).field("default_branch")
                    ))
                );
            } else {
                assert!(
                    !action.bindings.contains_key("branch"),
                    "{} must not consume nullable default_branch",
                    kind.id()
                );
            }
            if matches!(kind, ActionKind::GitFetch | ActionKind::GitFastForward) {
                match &action.step.action {
                    Action::GitFetch { branch, .. } | Action::GitFastForward { branch, .. } => {
                        assert_eq!(branch, "main")
                    }
                    _ => unreachable!(),
                }
            }
            assert_graph_authoring_round_trip(&format!("github-loop-{}", kind.id()), &graph);
        }
    }

    #[test]
    fn github_loop_never_guesses_bindings_for_non_git_actions() {
        let mut graph = WorkflowGraph::default();
        let list_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Action(ActionKind::GithubListRepositories),
        )
        .unwrap();
        let loop_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootAfter { node_id: list_id },
            Some(EdgePort::Success),
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();
        for kind in ActionKind::ALL.into_iter().filter(|kind| {
            kind.is_graph_action()
                && !matches!(
                    kind,
                    ActionKind::GitClone
                        | ActionKind::GitInspect
                        | ActionKind::GitCloneIfMissing
                        | ActionKind::GitFetch
                        | ActionKind::GitFastForward
                )
        }) {
            let id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::NestedStart {
                    scope: ComposerGraphNestedScope::ForEachBody {
                        owner_id: loop_id.clone(),
                    },
                },
                None,
                ComposerGraphBlockKind::Action(kind),
            )
            .unwrap();
            let GraphNode::Action(action) = graph_node(&graph, &id).unwrap() else {
                panic!("action expected")
            };
            assert!(
                action.bindings.is_empty(),
                "{} guessed repository bindings: {:?}",
                kind.id(),
                action.bindings
            );
        }
        assert_graph_authoring_round_trip("github-loop-non-git-actions", &graph);
    }

    #[test]
    fn nested_foreach_aliases_are_unique_editable_and_round_trip() {
        let mut graph = WorkflowGraph::default();
        let outer = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();
        let middle = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: outer.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();
        let inner = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: middle.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();
        graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: inner.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();

        let aliases = [&outer, &middle, &inner]
            .into_iter()
            .flat_map(|id| {
                let GraphNode::ForEach(node) = graph_node(&graph, id).unwrap() else {
                    panic!("foreach expected")
                };
                [node.item_alias.clone(), node.index_alias.clone().unwrap()]
            })
            .collect::<Vec<_>>();
        assert_eq!(aliases.iter().collect::<BTreeSet<_>>().len(), aliases.len());
        assert_eq!(
            aliases,
            [
                "item",
                "item_index",
                "item_2",
                "item_2_index",
                "item_3",
                "item_3_index"
            ]
        );

        let GraphNode::ForEach(node) = graph_node_mut(&mut graph, &inner).unwrap() else {
            panic!("foreach expected")
        };
        node.item_alias = "leaf".into();
        node.index_alias = Some("leaf_index".into());
        assert_graph_authoring_round_trip("nested-foreach-aliases", &graph);
    }

    fn populated_control_graph(kind: ComposerGraphBlockKind) -> (WorkflowGraph, String) {
        let mut graph = WorkflowGraph::default();
        let control_id =
            graph_insert_composer_block(&mut graph, &ComposerGraphAttach::RootStart, None, kind)
                .unwrap();
        match graph_node(&graph, &control_id).unwrap() {
            GraphNode::ForEach(_) => {
                graph_insert_composer_block(
                    &mut graph,
                    &ComposerGraphAttach::NestedStart {
                        scope: ComposerGraphNestedScope::ForEachBody {
                            owner_id: control_id.clone(),
                        },
                    },
                    None,
                    ComposerGraphBlockKind::Action(ActionKind::InspectPath),
                )
                .unwrap();
            }
            GraphNode::If(_) => {
                for scope in [
                    ComposerGraphNestedScope::IfThen {
                        owner_id: control_id.clone(),
                    },
                    ComposerGraphNestedScope::IfElse {
                        owner_id: control_id.clone(),
                    },
                ] {
                    graph_insert_composer_block(
                        &mut graph,
                        &ComposerGraphAttach::NestedStart { scope },
                        None,
                        ComposerGraphBlockKind::Action(ActionKind::InspectPath),
                    )
                    .unwrap();
                }
            }
            GraphNode::Switch(_) => {
                for scope in [
                    ComposerGraphNestedScope::SwitchCase {
                        owner_id: control_id.clone(),
                        case_id: "case-1".into(),
                    },
                    ComposerGraphNestedScope::SwitchDefault {
                        owner_id: control_id.clone(),
                    },
                ] {
                    graph_insert_composer_block(
                        &mut graph,
                        &ComposerGraphAttach::NestedStart { scope },
                        None,
                        ComposerGraphBlockKind::Action(ActionKind::InspectPath),
                    )
                    .unwrap();
                }
            }
            GraphNode::Action(_) | GraphNode::Join(_) => {
                panic!("expected nested control node")
            }
        }
        (graph, control_id)
    }

    #[test]
    fn graph_authoring_matrix_uses_every_explicit_output_port() {
        for port in [EdgePort::Success, EdgePort::Failure, EdgePort::Always] {
            let mut graph = WorkflowGraph::default();
            let source = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootStart,
                None,
                ComposerGraphBlockKind::Action(ActionKind::GitInspect),
            )
            .unwrap();
            graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootAfter {
                    node_id: source.clone(),
                },
                Some(port.clone()),
                ComposerGraphBlockKind::Action(ActionKind::InspectPath),
            )
            .unwrap();
            assert_graph_authoring_round_trip(&format!("action-port-{port:?}"), &graph);
        }

        for (kind, ports) in [
            (
                ComposerGraphBlockKind::ForEach,
                vec![EdgePort::Completed, EdgePort::Empty, EdgePort::Failure],
            ),
            (
                ComposerGraphBlockKind::If,
                vec![EdgePort::Completed, EdgePort::Failure],
            ),
            (
                ComposerGraphBlockKind::Switch,
                vec![EdgePort::Completed, EdgePort::Failure],
            ),
        ] {
            for port in ports {
                let (mut graph, control_id) = populated_control_graph(kind);
                graph_insert_composer_block(
                    &mut graph,
                    &ComposerGraphAttach::RootAfter {
                        node_id: control_id,
                    },
                    Some(port.clone()),
                    ComposerGraphBlockKind::Action(ActionKind::InspectPath),
                )
                .unwrap();
                assert_graph_authoring_round_trip(
                    &format!("control-{}-{port:?}", graph.nodes[0].kind_name()),
                    &graph,
                );
            }
        }

        for port in [EdgePort::Completed, EdgePort::Failure] {
            let action = |kind, id| {
                GraphNode::Action(Box::new(ActionNode {
                    step: default_step(kind, id).unwrap(),
                    bindings: BTreeMap::new(),
                }))
            };
            let mut graph = WorkflowGraph {
                entries: vec!["left".into(), "right".into()],
                nodes: vec![
                    action(ActionKind::GitInspect, "left"),
                    action(ActionKind::InspectPath, "right"),
                    GraphNode::Join(JoinNode {
                        id: "join".into(),
                        mode: JoinMode::All,
                    }),
                ],
                edges: vec![
                    GraphEdge::new("left", EdgePort::Success, "join"),
                    GraphEdge::new("right", EdgePort::Success, "join"),
                ],
                ..WorkflowGraph::default()
            };
            graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootAfter {
                    node_id: "join".into(),
                },
                Some(port.clone()),
                ComposerGraphBlockKind::Action(ActionKind::InspectPath),
            )
            .unwrap();
            assert_graph_authoring_round_trip(&format!("join-port-{port:?}"), &graph);
        }
    }

    #[test]
    fn graph_authoring_rejects_invalid_ports_atomically_for_every_node_kind() {
        let mut cases = Vec::new();

        let mut action = WorkflowGraph::default();
        let action_id = graph_insert_composer_block(
            &mut action,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Action(ActionKind::GitInspect),
        )
        .unwrap();
        cases.push((action, action_id, EdgePort::Empty));

        for kind in [
            ComposerGraphBlockKind::ForEach,
            ComposerGraphBlockKind::If,
            ComposerGraphBlockKind::Switch,
        ] {
            let (graph, id) = populated_control_graph(kind);
            cases.push((graph, id, EdgePort::Success));
        }

        let action = |kind, id| {
            GraphNode::Action(Box::new(ActionNode {
                step: default_step(kind, id).unwrap(),
                bindings: BTreeMap::new(),
            }))
        };
        cases.push((
            WorkflowGraph {
                entries: vec!["left".into(), "right".into()],
                nodes: vec![
                    action(ActionKind::GitInspect, "left"),
                    action(ActionKind::InspectPath, "right"),
                    GraphNode::Join(JoinNode {
                        id: "join".into(),
                        mode: JoinMode::All,
                    }),
                ],
                edges: vec![
                    GraphEdge::new("left", EdgePort::Success, "join"),
                    GraphEdge::new("right", EdgePort::Success, "join"),
                ],
                ..WorkflowGraph::default()
            },
            "join".into(),
            EdgePort::Success,
        ));

        for (mut graph, source, invalid_port) in cases {
            let before = serde_yaml::to_string(&graph).unwrap();
            let error = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootAfter { node_id: source },
                Some(invalid_port),
                ComposerGraphBlockKind::Action(ActionKind::InspectPath),
            )
            .unwrap_err();
            assert!(error.contains("недоступен"));
            assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        }
    }

    #[test]
    fn stale_nested_after_does_not_materialize_optional_scopes() {
        for (kind, scope) in [
            (
                ComposerGraphBlockKind::If,
                ComposerGraphNestedScope::IfElse {
                    owner_id: "if-1".into(),
                },
            ),
            (
                ComposerGraphBlockKind::Switch,
                ComposerGraphNestedScope::SwitchDefault {
                    owner_id: "switch-1".into(),
                },
            ),
        ] {
            let mut graph = WorkflowGraph::default();
            let owner = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootStart,
                None,
                kind,
            )
            .unwrap();
            assert_eq!(owner, scope.owner_id());
            let before = serde_yaml::to_string(&graph).unwrap();
            let error = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::NestedAfter {
                    scope,
                    node_id: "stale-child".into(),
                },
                Some(EdgePort::Success),
                ComposerGraphBlockKind::Action(ActionKind::InspectPath),
            )
            .unwrap_err();
            assert!(error.contains("вложенная область"));
            assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        }
    }

    #[test]
    fn incomplete_if_and_switch_block_downstream_attach_atomically() {
        for kind in [ComposerGraphBlockKind::If, ComposerGraphBlockKind::Switch] {
            let mut graph = WorkflowGraph::default();
            let control_id = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootStart,
                None,
                kind,
            )
            .unwrap();
            let before = serde_yaml::to_string(&graph).unwrap();
            let error = graph_insert_composer_block(
                &mut graph,
                &ComposerGraphAttach::RootAfter {
                    node_id: control_id,
                },
                Some(EdgePort::Completed),
                ComposerGraphBlockKind::Action(ActionKind::InspectPath),
            )
            .unwrap_err();
            assert!(error.contains("вет"), "unexpected blocker message: {error}");
            assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        }

        let mut conditional = WorkflowGraph::default();
        let if_id = graph_insert_composer_block(
            &mut conditional,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::If,
        )
        .unwrap();
        let GraphNode::If(node) = graph_node(&conditional, &if_id).unwrap() else {
            panic!("if node expected")
        };
        assert!(node.else_graph.is_none());
        graph_insert_composer_block(
            &mut conditional,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::IfThen {
                    owner_id: if_id.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let after_if = ComposerGraphAttach::RootAfter {
            node_id: if_id.clone(),
        };
        assert!(graph_attach_blocker(&conditional, &after_if).is_none());
        let GraphNode::If(node) = graph_node_mut(&mut conditional, &if_id).unwrap() else {
            panic!("if node expected")
        };
        node.else_graph = Some(Box::new(WorkflowGraph::default()));
        assert!(graph_attach_blocker(&conditional, &after_if)
            .unwrap()
            .contains("Иначе"));

        let mut selection = WorkflowGraph::default();
        let switch_id = graph_insert_composer_block(
            &mut selection,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Switch,
        )
        .unwrap();
        let GraphNode::Switch(node) = graph_node(&selection, &switch_id).unwrap() else {
            panic!("switch node expected")
        };
        assert!(node.default.is_none());
        graph_insert_composer_block(
            &mut selection,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::SwitchCase {
                    owner_id: switch_id.clone(),
                    case_id: "case-1".into(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let after_switch = ComposerGraphAttach::RootAfter {
            node_id: switch_id.clone(),
        };
        assert!(graph_attach_blocker(&selection, &after_switch).is_none());
        let GraphNode::Switch(node) = graph_node_mut(&mut selection, &switch_id).unwrap() else {
            panic!("switch node expected")
        };
        node.default = Some(Box::new(WorkflowGraph::default()));
        assert!(graph_attach_blocker(&selection, &after_switch)
            .unwrap()
            .contains("По умолчанию"));
    }

    #[test]
    fn join_blocks_downstream_until_two_valid_inputs_exist() {
        let action = |kind, id| {
            GraphNode::Action(Box::new(ActionNode {
                step: default_step(kind, id).unwrap(),
                bindings: BTreeMap::new(),
            }))
        };
        let mut graph = WorkflowGraph {
            entries: vec!["left".into(), "right".into()],
            nodes: vec![
                action(ActionKind::GitInspect, "left"),
                action(ActionKind::InspectPath, "right"),
                GraphNode::Join(JoinNode {
                    id: "join".into(),
                    mode: JoinMode::All,
                }),
            ],
            edges: vec![GraphEdge::new("left", EdgePort::Success, "join")],
            ..WorkflowGraph::default()
        };
        let attach = ComposerGraphAttach::RootAfter {
            node_id: "join".into(),
        };
        let before = serde_yaml::to_string(&graph).unwrap();
        let error = graph_insert_composer_block(
            &mut graph,
            &attach,
            Some(EdgePort::Completed),
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap_err();
        assert!(error.contains("минимум две"));
        assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);

        graph_set_incoming_edge(&mut graph, "join", "right", Some(EdgePort::Success)).unwrap();
        assert!(graph_attach_blocker(&graph, &attach).is_none());
        graph_insert_composer_block(
            &mut graph,
            &attach,
            Some(EdgePort::Completed),
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        assert_graph_authoring_round_trip("join-after-two-inputs", &graph);
    }

    #[test]
    fn join_rejects_downstream_incoming_source_atomically() {
        let action = |kind, id| {
            GraphNode::Action(Box::new(ActionNode {
                step: default_step(kind, id).unwrap(),
                bindings: BTreeMap::new(),
            }))
        };
        let mut graph = WorkflowGraph {
            entries: vec!["left".into(), "right".into()],
            nodes: vec![
                action(ActionKind::GitInspect, "left"),
                action(ActionKind::InspectPath, "right"),
                GraphNode::Join(JoinNode {
                    id: "join".into(),
                    mode: JoinMode::All,
                }),
                action(ActionKind::InspectPath, "child"),
            ],
            edges: vec![
                GraphEdge::new("left", EdgePort::Success, "join"),
                GraphEdge::new("right", EdgePort::Success, "join"),
                GraphEdge::new("join", EdgePort::Completed, "child"),
            ],
            ..WorkflowGraph::default()
        };
        graph.validate().unwrap();
        let before = serde_yaml::to_string(&graph).unwrap();
        let error = graph_set_incoming_edge(&mut graph, "join", "child", Some(EdgePort::Success))
            .unwrap_err();
        assert!(error.contains("цикл"));
        assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        graph.validate().unwrap();
    }

    #[test]
    fn foreach_requires_per_item_body_before_after_loop_and_git_clone_is_item_scoped() {
        let mut graph = WorkflowGraph::default();
        let list_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Action(ActionKind::GithubListRepositories),
        )
        .unwrap();
        let loop_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootAfter {
                node_id: list_id.clone(),
            },
            Some(EdgePort::Success),
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();

        let after = ComposerGraphAttach::RootAfter {
            node_id: loop_id.clone(),
        };
        let node_count = graph.nodes.len();
        let error = graph_insert_composer_block(
            &mut graph,
            &after,
            Some(EdgePort::Completed),
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap_err();
        assert!(error.contains("Для каждого item"));
        assert_eq!(graph.nodes.len(), node_count);

        let clone_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: loop_id.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::GitClone),
        )
        .unwrap();
        assert!(graph_attach_blocker(&graph, &after).is_none());

        let GraphNode::Action(clone) = graph_node(&graph, &clone_id).unwrap() else {
            panic!("clone action node expected");
        };
        assert_eq!(
            clone.bindings.get("repo"),
            Some(&Binding::field(
                FieldRef::loop_item(&loop_id).field("https_url")
            ))
        );
        assert_eq!(
            clone.bindings.get("dest"),
            Some(&Binding::interpolated([
                TemplatePart::literal("$HOME/Developer/"),
                TemplatePart::field(FieldRef::loop_item(&loop_id).field("full_name")),
            ]))
        );
        assert_eq!(
            clone.bindings.get("branch"),
            Some(&Binding::field(
                FieldRef::loop_item(&loop_id).field("default_branch")
            ))
        );
        graph.validate().unwrap();
    }

    #[test]
    fn graph_validation_errors_are_actionable_russian_and_collapse_empty_loop_duplicates() {
        let mut list = default_step(
            ActionKind::GithubListRepositories,
            "github-list-repositories-1",
        )
        .unwrap();
        list.auth = AuthPolicy::GitCredential;
        let graph = WorkflowGraph {
            entries: vec![list.id.clone()],
            nodes: vec![
                GraphNode::Action(Box::new(ActionNode {
                    step: list,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::ForEach(ForEachNode {
                    id: "for-each-2".into(),
                    collection: Binding::literal(serde_json::json!([])),
                    item_alias: "repository".into(),
                    index_alias: Some("index".into()),
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(WorkflowGraph::default()),
                }),
            ],
            edges: vec![GraphEdge::new(
                "github-list-repositories-1",
                EdgePort::Success,
                "for-each-2",
            )],
            ..WorkflowGraph::default()
        };
        let task = Task {
            id: "custom-scenario".into(),
            name: "Новый сценарий".into(),
            description: "test".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            graph: Some(graph.clone()),
            steps: Vec::new(),
        };
        let error = validate_graph_for_ui(&task, &graph).unwrap_err();
        assert!(error.contains("Сценарий «Новый сценарий» пока не готов"));
        assert!(error.contains("недопустимая политика безопасности"));
        assert!(error.contains("Добавьте действие через «＋ Для каждого item»"));
        assert_eq!(
            error.matches("Цикл «Для каждого repository» пуст").count(),
            1
        );
        assert!(!error.contains("invalid workflow graph"));
        assert!(!error.contains("graph.node["));
    }

    #[test]
    fn empty_branch_validation_names_each_visual_scope() {
        let mut graph = WorkflowGraph::default();
        let if_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::If,
        )
        .unwrap();
        let switch_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Switch,
        )
        .unwrap();
        let GraphNode::If(node) = graph_node_mut(&mut graph, &if_id).unwrap() else {
            panic!("if node expected")
        };
        node.else_graph = Some(Box::new(WorkflowGraph::default()));
        let GraphNode::Switch(node) = graph_node_mut(&mut graph, &switch_id).unwrap() else {
            panic!("switch node expected")
        };
        node.default = Some(Box::new(WorkflowGraph::default()));

        let task = graph_authoring_test_task("empty-branches", graph.clone());
        let error = validate_graph_for_ui(&task, &graph).unwrap_err();
        for expected in [
            "Ветка «Тогда»",
            "Ветка «Иначе»",
            "Вариант «case-1»",
            "Ветка «По умолчанию»",
        ] {
            assert!(error.contains(expected), "missing {expected}: {error}");
            assert_eq!(error.matches(expected).count(), 1);
        }
        assert!(!error.contains("graph.node["));
    }

    #[test]
    fn policy_diagnostics_do_not_mutate_loaded_step_and_explicit_reset_repairs_it() {
        let mut step = default_step(
            ActionKind::GithubListRepositories,
            "github-list-repositories-1",
        )
        .unwrap();
        step.auth = AuthPolicy::GitCredential;
        step.allow_elevation = ElevationPolicy::Allow;
        step.dangerous = true;
        let definition = definition_for_action(&step.action);
        let before = serde_yaml::to_string(&step).unwrap();

        let issues = graph_step_policy_issues(&step, &definition.policy);
        assert_eq!(issues.len(), 3);
        assert_eq!(serde_yaml::to_string(&step).unwrap(), before);
        assert!(!definition.policy.accepts(&step));

        reset_graph_step_policy(&mut step, &definition.policy);
        assert!(definition.policy.accepts(&step));
        assert!(matches!(step.auth, AuthPolicy::None));
        assert!(matches!(step.allow_elevation, ElevationPolicy::Forbidden));
        assert!(!step.dangerous);
        step.validate().unwrap();
    }

    #[test]
    fn optional_object_inputs_are_atomic_while_required_objects_keep_leaf_editing() {
        let schema = ObjectSchema::new("test.input@1").with_field(
            "parent",
            FieldSchema::optional(ContextType::object(
                ObjectSchema::new("test.parent@1")
                    .with_field(
                        "value",
                        FieldSchema::required(ContextType::String { format: None }),
                    )
                    .with_field(
                        "nullable_value",
                        FieldSchema::required(ContextType::String { format: None }).nullable(),
                    ),
            ))
            .nullable(),
        );
        let fields = graph_input_fields(&schema)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let parent = fields.get("parent").unwrap();
        assert!(!parent.required);
        assert!(parent.nullable);
        assert!(matches!(parent.value_type, ContextType::Object { .. }));
        assert!(!fields.contains_key("parent.value"));
        assert!(!fields.contains_key("parent.nullable_value"));

        let required_schema = ObjectSchema::new("test.required-input@1").with_field(
            "parent",
            FieldSchema::required(ContextType::object(
                ObjectSchema::new("test.required-parent@1").with_field(
                    "nullable_value",
                    FieldSchema::required(ContextType::String { format: None }).nullable(),
                ),
            )),
        );
        let fields = graph_input_fields(&required_schema)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let nullable = fields.get("parent.nullable_value").unwrap();
        assert!(nullable.required);
        assert!(nullable.nullable);
    }

    #[test]
    fn install_dmg_identity_is_offered_and_materialized_as_one_complete_object() {
        let definition = block_definition(ActionKind::InstallDmg);
        let fields = graph_input_fields(&definition.input_schema)
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert!(fields.contains_key("identity"));
        for partial in [
            "identity.bundle_identifier",
            "identity.team_identifier",
            "identity.version",
        ] {
            assert!(
                !fields.contains_key(partial),
                "partial object leaf {partial} leaked"
            );
        }

        let step = default_step(ActionKind::InstallDmg, "install").unwrap();
        let materialized = ppduster::automation::materialize_step(
            &step,
            &BTreeMap::from([(
                "identity".into(),
                Binding::literal(serde_json::json!({
                    "bundle_identifier": "com.example.Application",
                    "team_identifier": "TEAM123456",
                    "version": "1.2.3"
                })),
            )]),
            &ppduster::automation::ContextStore::default(),
            ppduster::automation::BindingLimits::default(),
        )
        .unwrap();
        let Action::InstallDmg {
            identity: Some(identity),
            ..
        } = materialized.action
        else {
            panic!("complete identity object was not materialized")
        };
        assert_eq!(identity.bundle_identifier, "com.example.Application");
        assert_eq!(identity.team_identifier, "TEAM123456");
        assert_eq!(identity.version, "1.2.3");
    }

    #[test]
    fn enum_input_contracts_drive_all_generic_choices_and_materialize() {
        let cases: &[(ActionKind, &str, &[&str])] = &[
            (
                ActionKind::InspectPath,
                "expect.kind",
                &["file", "directory", "symlink", "other"],
            ),
            (ActionKind::WriteFile, "on_conflict", &["fail", "replace"]),
            (ActionKind::RunCommand, "shell", &["forbidden", "allow"]),
            (
                ActionKind::RunScript,
                "interpreter",
                &["sh", "bash", "powershell"],
            ),
            (
                ActionKind::ExtractArchive,
                "format",
                &["auto", "zip", "tar", "tar-gz", "tar-bz2", "tar-xz"],
            ),
            (
                ActionKind::AppStoreInstall,
                "operation",
                &["install", "get"],
            ),
            (
                ActionKind::BambuStudioRelease,
                "channel",
                &["release", "beta"],
            ),
            (ActionKind::ActivateLicense, "provider", &["light-burn"]),
            (ActionKind::ActivateLicense, "method", &["vendor-ui"]),
        ];
        let mut literal_count = 0usize;
        for (kind, target, expected) in cases {
            let definition = block_definition(*kind);
            let fields = graph_input_fields(&definition.input_schema)
                .into_iter()
                .collect::<BTreeMap<_, _>>();
            let field = fields
                .get(*target)
                .unwrap_or_else(|| panic!("{} missing enum input {target}", kind.id()));
            assert_eq!(
                field
                    .allowed_values
                    .iter()
                    .map(|value| value.as_str().unwrap())
                    .collect::<Vec<_>>(),
                *expected
            );
            assert!(validate_literal_binding(&serde_json::json!("not-an-option"), field).is_err());

            for value in &field.allowed_values {
                literal_count += 1;
                validate_literal_binding(value, field).unwrap();
                let mut step =
                    default_step(*kind, format!("{}-{literal_count}", kind.id())).unwrap();
                apply_literal_policy_implications(&mut step, target, value);
                let materialized = ppduster::automation::materialize_step(
                    &step,
                    &BTreeMap::from([(target.to_string(), Binding::literal(value.clone()))]),
                    &ppduster::automation::ContextStore::default(),
                    ppduster::automation::BindingLimits::default(),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "{} {target}={value} did not materialize: {error}",
                        kind.id()
                    )
                });
                materialized.validate().unwrap();
            }
        }
        assert_eq!(literal_count, 23);
    }

    #[test]
    fn every_manual_input_starts_from_a_materializable_valid_value() {
        let mut offered = 0usize;
        for kind in ActionKind::ALL
            .into_iter()
            .filter(|kind| kind.is_graph_action())
        {
            let step = default_step(kind, format!("manual-{}", kind.id())).unwrap();
            let definition = block_definition(kind);
            for (target, field) in graph_input_fields(&definition.input_schema) {
                offered += 1;
                let value = manual_input_initial_value(&step, &target, &field);
                validate_literal_binding(&value, &field).unwrap_or_else(|error| {
                    panic!(
                        "{} {target} manual prototype {value} invalid: {error}",
                        kind.id()
                    )
                });
                let materialized = ppduster::automation::materialize_step(
                    &step,
                    &BTreeMap::from([(target.clone(), Binding::literal(value.clone()))]),
                    &ppduster::automation::ContextStore::default(),
                    ppduster::automation::BindingLimits::default(),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "{} {target} manual prototype {value} does not materialize: {error}",
                        kind.id()
                    )
                });
                materialized.validate().unwrap_or_else(|error| {
                    panic!(
                        "{} {target} manual prototype is invalid: {error}",
                        kind.id()
                    )
                });
            }
        }
        // The raw leaf schema has 77 inputs. The three required identity
        // leaves are intentionally represented by one atomic object control,
        // eliminating two unsafe partial controls.
        assert_eq!(offered, 75, "update the manual-input authoring matrix");
    }

    #[test]
    fn switch_case_defaults_follow_scalar_selector_type() {
        for kind in GraphSwitchScalarKind::ALL {
            let value = kind.default_value(2);
            assert!(
                kind.accepts(&value),
                "{} default must be compatible",
                kind.label()
            );
            assert_eq!(
                graph_switch_selector_kind(&Binding::literal(value), &[]),
                Some(kind)
            );
        }
        assert!(GraphSwitchScalarKind::Number.accepts(&serde_json::json!(2)));
        assert!(!GraphSwitchScalarKind::Integer.accepts(&serde_json::json!(2.5)));

        let null_selector = Binding::literal(serde_json::Value::Null);
        let before = serde_yaml::to_string(&null_selector).unwrap();
        assert_eq!(
            graph_switch_selector_kind(&null_selector, &[]),
            Some(GraphSwitchScalarKind::Null)
        );
        assert_eq!(serde_yaml::to_string(&null_selector).unwrap(), before);

        let composite_selector = Binding::literal(serde_json::json!({ "key": "value" }));
        let before = serde_yaml::to_string(&composite_selector).unwrap();
        assert_eq!(graph_switch_selector_kind(&composite_selector, &[]), None);
        assert_eq!(serde_yaml::to_string(&composite_selector).unwrap(), before);
    }

    #[test]
    fn switch_case_ids_reuse_first_free_without_collisions_and_round_trip() {
        let mut graph = WorkflowGraph::default();
        let list = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Action(ActionKind::GithubListRepositories),
        )
        .unwrap();
        let loop_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootAfter { node_id: list },
            Some(EdgePort::Success),
            ComposerGraphBlockKind::ForEach,
        )
        .unwrap();
        let switch_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::ForEachBody {
                    owner_id: loop_id.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Switch,
        )
        .unwrap();
        graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::SwitchCase {
                    owner_id: switch_id.clone(),
                    case_id: "case-1".into(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();

        let branch = |id: &str| {
            Box::new(WorkflowGraph {
                entries: vec![id.into()],
                nodes: vec![GraphNode::Action(Box::new(ActionNode {
                    step: default_step(ActionKind::InspectPath, id).unwrap(),
                    bindings: BTreeMap::new(),
                }))],
                ..WorkflowGraph::default()
            })
        };
        let GraphNode::Switch(node) = graph_node_mut(&mut graph, &switch_id).unwrap() else {
            panic!("switch expected")
        };
        node.selector = Binding::field(FieldRef::loop_item(&loop_id).field("default_branch"));
        let case_id = first_free_switch_case_id(&node.cases);
        node.cases.push(SwitchCase {
            id: case_id,
            values: vec![serde_json::json!("develop")],
            graph: branch("case-action-2"),
        });
        let case_id = first_free_switch_case_id(&node.cases);
        node.cases.push(SwitchCase {
            id: case_id,
            values: vec![serde_json::json!("release")],
            graph: branch("case-action-3"),
        });
        node.cases.remove(1);
        assert_eq!(first_free_switch_case_id(&node.cases), "case-2");
        let case_id = first_free_switch_case_id(&node.cases);
        node.cases.push(SwitchCase {
            id: case_id,
            values: vec![serde_json::json!("feature")],
            graph: branch("case-action-4"),
        });
        assert_eq!(first_free_switch_case_id(&node.cases), "case-4");
        let case_id = first_free_switch_case_id(&node.cases);
        node.cases.push(SwitchCase {
            id: case_id,
            values: vec![serde_json::Value::Null],
            graph: branch("case-action-null"),
        });
        assert_eq!(
            node.cases
                .iter()
                .map(|case| case.id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            node.cases.len()
        );
        assert_graph_authoring_round_trip("switch-first-free-case-ids", &graph);
    }

    #[test]
    fn switch_case_values_reuse_first_free_globally_for_every_scalar_kind() {
        fn branch(id: &str) -> Box<WorkflowGraph> {
            Box::new(WorkflowGraph {
                entries: vec![id.into()],
                nodes: vec![GraphNode::Action(Box::new(ActionNode {
                    step: default_step(ActionKind::InspectPath, id).unwrap(),
                    bindings: BTreeMap::new(),
                }))],
                ..WorkflowGraph::default()
            })
        }

        for kind in GraphSwitchScalarKind::ALL {
            let first = first_free_switch_case_value(kind, &[]).unwrap();
            let mut cases = vec![SwitchCase {
                id: "case-1".into(),
                values: vec![first.clone()],
                graph: branch(&format!("{}-case-1", kind.label())),
            }];
            if let Some(second) = first_free_switch_case_value(kind, &cases) {
                assert_ne!(second, first);
                cases.push(SwitchCase {
                    id: "case-2".into(),
                    values: vec![second],
                    graph: branch(&format!("{}-case-2", kind.label())),
                });
            }

            cases.remove(0);
            assert_eq!(
                first_free_switch_case_value(kind, &cases),
                Some(first.clone())
            );
            cases.push(SwitchCase {
                id: "case-readded".into(),
                values: vec![first],
                graph: branch(&format!("{}-case-readded", kind.label())),
            });

            if let Some(extra) = first_free_switch_case_value(kind, &cases) {
                assert!(cases
                    .iter()
                    .all(|case| !case.values.iter().any(|value| value == &extra)));
                cases[0].values.push(extra);
            }
            let values = cases
                .iter()
                .flat_map(|case| case.values.iter())
                .collect::<Vec<_>>();
            for (index, value) in values.iter().enumerate() {
                assert!(!values[index + 1..].contains(value));
            }

            let graph = WorkflowGraph {
                entries: vec![format!("switch-{}", kind.label())],
                nodes: vec![GraphNode::Switch(SwitchNode {
                    id: format!("switch-{}", kind.label()),
                    selector: Binding::literal(kind.default_value(0)),
                    cases,
                    default: None,
                })],
                ..WorkflowGraph::default()
            };
            assert_graph_authoring_round_trip(
                &format!("switch-first-free-{}", kind.label()),
                &graph,
            );
        }
    }

    #[test]
    fn optional_control_branches_can_only_be_removed_when_empty() {
        let mut graph = WorkflowGraph::default();
        let if_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::If,
        )
        .unwrap();
        graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::IfThen {
                    owner_id: if_id.clone(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let else_scope = ComposerGraphNestedScope::IfElse {
            owner_id: if_id.clone(),
        };
        graph_nested_scope_mut(&mut graph, &else_scope).unwrap();
        assert!(graph_remove_optional_scope(&mut graph, &else_scope).unwrap());
        let GraphNode::If(node) = graph_node(&graph, &if_id).unwrap() else {
            panic!("if expected")
        };
        assert!(node.else_graph.is_none());

        let else_child = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: else_scope.clone(),
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let before = serde_yaml::to_string(&graph).unwrap();
        let error = graph_remove_optional_scope(&mut graph, &else_scope).unwrap_err();
        assert!(error.contains("Иначе") && error.contains("не пуста"));
        assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        assert!(graph_remove_composer_node(&mut graph, &else_child).unwrap());
        assert!(graph_remove_optional_scope(&mut graph, &else_scope).unwrap());

        let switch_id = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::RootStart,
            None,
            ComposerGraphBlockKind::Switch,
        )
        .unwrap();
        graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: ComposerGraphNestedScope::SwitchCase {
                    owner_id: switch_id.clone(),
                    case_id: "case-1".into(),
                },
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let default_scope = ComposerGraphNestedScope::SwitchDefault {
            owner_id: switch_id.clone(),
        };
        graph_nested_scope_mut(&mut graph, &default_scope).unwrap();
        assert!(graph_remove_optional_scope(&mut graph, &default_scope).unwrap());
        let default_child = graph_insert_composer_block(
            &mut graph,
            &ComposerGraphAttach::NestedStart {
                scope: default_scope.clone(),
            },
            None,
            ComposerGraphBlockKind::Action(ActionKind::InspectPath),
        )
        .unwrap();
        let before = serde_yaml::to_string(&graph).unwrap();
        let error = graph_remove_optional_scope(&mut graph, &default_scope).unwrap_err();
        assert!(error.contains("По умолчанию") && error.contains("не пуста"));
        assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);
        assert!(graph_remove_composer_node(&mut graph, &default_child).unwrap());
        assert!(graph_remove_optional_scope(&mut graph, &default_scope).unwrap());
        assert_graph_authoring_round_trip("optional-branches-removed", &graph);
    }

    #[test]
    fn switch_case_removal_never_discards_a_nonempty_branch() {
        let branch = |id: &str| {
            Box::new(WorkflowGraph {
                entries: vec![id.into()],
                nodes: vec![GraphNode::Action(Box::new(ActionNode {
                    step: default_step(ActionKind::InspectPath, id).unwrap(),
                    bindings: BTreeMap::new(),
                }))],
                ..WorkflowGraph::default()
            })
        };
        let mut graph = WorkflowGraph {
            entries: vec!["switch".into()],
            nodes: vec![GraphNode::Switch(SwitchNode {
                id: "switch".into(),
                selector: Binding::literal("selector"),
                cases: vec![
                    SwitchCase {
                        id: "case-1".into(),
                        values: vec![serde_json::json!("one")],
                        graph: branch("case-child-1"),
                    },
                    SwitchCase {
                        id: "case-2".into(),
                        values: vec![serde_json::json!("two")],
                        graph: branch("case-child-2"),
                    },
                ],
                default: None,
            })],
            ..WorkflowGraph::default()
        };
        let before = serde_yaml::to_string(&graph).unwrap();
        let error = graph_remove_switch_case(&mut graph, "switch", "case-2").unwrap_err();
        assert!(error.contains("case-2") && error.contains("не пуст"));
        assert_eq!(serde_yaml::to_string(&graph).unwrap(), before);

        assert!(graph_remove_composer_node(&mut graph, "case-child-2").unwrap());
        assert!(graph_remove_switch_case(&mut graph, "switch", "case-2").unwrap());
        assert_graph_authoring_round_trip("safe-switch-case-removal", &graph);
    }

    #[test]
    fn nullable_switch_field_accepts_typed_and_null_case_values() {
        let selector = Binding::field(FieldRef::step("source").field("branch"));
        let options = vec![ComposerGraphBindingOption {
            label: "source.branch".into(),
            binding: selector.clone(),
            value_type: ContextType::String { format: None },
            required: false,
            nullable: true,
            sensitivity: Sensitivity::Public,
        }];
        let contract = graph_switch_selector_contract(&selector, &options);
        assert_eq!(contract, Some((GraphSwitchScalarKind::String, true)));
        let (kind, nullable) = contract.unwrap();
        assert!(graph_switch_case_value_compatible(
            kind,
            nullable,
            &serde_json::Value::Null
        ));
        assert!(graph_switch_case_value_compatible(
            kind,
            nullable,
            &serde_json::json!("main")
        ));
        assert!(!graph_switch_case_value_compatible(
            kind,
            nullable,
            &serde_json::json!(true)
        ));
    }

    #[test]
    fn github_picker_rejects_noncanonical_v3_graphs_without_mutating_them() {
        let pack = load_tasks().unwrap();
        let canonical = standalone_github_picker_task(&pack);
        assert!(github_picker_source_steps(&canonical).is_some());

        let mut failure_edge = canonical.clone();
        failure_edge.graph.as_mut().unwrap().edges[0].from.port = EdgePort::Failure;
        let before = serde_yaml::to_string(&failure_edge).unwrap();
        assert!(github_picker_source_steps(&failure_edge).is_none());
        assert_eq!(serde_yaml::to_string(&failure_edge).unwrap(), before);

        let mut bound = canonical.clone();
        let GraphNode::Action(action) = &mut bound.graph.as_mut().unwrap().nodes[0] else {
            panic!("expected action")
        };
        action.bindings.insert(
            "repo".into(),
            Binding::literal("https://example.invalid/repo.git"),
        );
        assert!(github_picker_source_steps(&bound).is_none());

        let mut spoofed = canonical;
        spoofed.id = "custom-git-workflow".into();
        assert!(github_picker_source_steps(&spoofed).is_none());
    }

    #[test]
    fn github_authentication_failure_offers_recovery_but_rate_limit_does_not() {
        assert!(github_errors_need_authorization(&[String::from(
            "GitHub repository discovery failed: GitHub CLI is not authenticated for github.com; run gh auth login"
        )]));
        assert!(!github_errors_need_authorization(&[String::from(
            "GitHub API rate limit was exceeded"
        )]));
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
