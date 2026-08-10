use anyhow::Context;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, Vec2,
};
use ppduster::automation::PackTrust;
use ppduster::automation::{
    block_definition, definition_for_action, describe_step, run_task, Action, ActionKind,
    AuthPolicy, ComparisonOperator, ContextPathSegment, ContextScope, CopyPathAction,
    CreateDirectoryAction, ExpressionLimits, ExpressionV1, ExpressionValue, FieldRef, GraphNode,
    IndeterminatePolicy, InspectPathAction, ObjectSchema, ReferenceV1, ReleaseChannel,
    RemovePathAction, RuleOutcomePolicy, RunOptions, RunReport, ScriptInterpreter, SemanticFormat,
    Sensitivity, Step, StepCondition, StepStatus, Task, TaskFile, TaskPack, TaskSource,
    TrustRequirement, WorkflowGraph, WriteConflictPolicy, WriteFileAction,
};
use ppduster::automation::{ContextType, FieldSchema};
use ppduster::github::{list_accessible_repositories, login_via_web, GithubRepository};
use regex::RegexBuilder;
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
        task: Box<Task>,
    },
}

impl ScenarioProject {
    fn scenario(&self, path: &[usize]) -> Option<&Task> {
        project_entry(&self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task.as_ref()),
            ProjectEntry::Group { .. } => None,
        })
    }

    fn scenario_mut(&mut self, path: &[usize]) -> Option<&mut Task> {
        project_entry_mut(&mut self.entries, path).and_then(|entry| match entry {
            ProjectEntry::Scenario { task } => Some(task.as_mut()),
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
struct ComposerArraySource {
    step_id: String,
    step_name: String,
    path: String,
    item: String,
    item_type: ContextType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposerLoopSource {
    step_id: String,
    step_name: String,
    item: String,
    fields: Vec<String>,
    item_type: ContextType,
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

fn composer_array_sources(task: &Task, before_index: usize) -> Vec<ComposerArraySource> {
    let mut sources = Vec::new();
    for step in task.steps.iter().take(before_index) {
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

fn composer_loop_sources(task: &Task, before_index: usize) -> Vec<ComposerLoopSource> {
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
                let item_type = composer_array_sources(task, index)
                    .into_iter()
                    .find(|source| source.step_id == *source_step && source.path == *array_path)
                    .map(|source| project_item_type(&source.item_type, fields))
                    .unwrap_or(ContextType::Any);
                Some(ComposerLoopSource {
                    step_id: step.id.clone(),
                    step_name: step_title(step),
                    item: item.clone(),
                    fields: fields.clone(),
                    item_type,
                })
            }
            _ => None,
        })
        .collect()
}

fn composer_condition_fields(task: &Task, before_index: usize) -> Vec<ComposerConditionField> {
    let mut fields = Vec::new();
    for step in task.steps.iter().take(before_index) {
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
            graph: None,
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
        let mut new_step = composer_step(kind, id.clone());
        if matches!(kind, ComposerBlockKind::ForEach) {
            let source = composer_array_sources(task, task.steps.len())
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
            task: Box::new(Task {
                id: format!("scenario-{ordinal}"),
                name: format!("Новый сценарий {ordinal}"),
                description: "Сценарий, собранный из атомарных операций в ppduster.".into(),
                platform: ppduster::rules::Platform::Macos,
                trust: TrustRequirement::ExternalAllowed,
                scenarios: Vec::new(),
                resolved_scenarios: Vec::new(),
                graph: None,
                steps: Vec::new(),
            }),
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
            task: Box::new(github_repository_composer_task(ordinal)),
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
                            let definition = block_definition(kind.action_kind());
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
                                            .wrap(),
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
            entries: vec![ProjectEntry::Scenario {
                task: Box::new(task),
            }],
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
                let array_sources = composer_array_sources(task, index);
                let loop_sources = composer_loop_sources(task, index);
                let condition_fields = composer_condition_fields(task, index);
                changed |= paint_composer_step_editor(
                    ui,
                    &mut task.steps[index],
                    &array_sources,
                    &loop_sources,
                    self.dark,
                );
                ui.add_space(12.0);
                changed |= paint_composer_conditions(
                    ui,
                    &mut task.steps[index],
                    &condition_fields,
                    self.dark,
                );
                ui.add_space(12.0);
                section_label(ui, "ВЫХОДНОЙ КОНТЕКСТ");
                for line in composer_step_context_lines(task, index) {
                    ui.add(
                        egui::Label::new(RichText::new(line).monospace().size(8.0).color(PURPLE))
                            .wrap(),
                    );
                }
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
        graph: None,
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

    lines.retain(|line| !line.starts_with("loop.items[]"));
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
            ui.add(
                egui::Label::new(RichText::new(yaml).monospace().size(8.0).color(text(dark)))
                    .wrap(),
            );
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

fn paint_composer_step_editor(
    ui: &mut egui::Ui,
    step: &mut Step,
    array_sources: &[ComposerArraySource],
    loop_sources: &[ComposerLoopSource],
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
            ui.label(
                RichText::new(format!(
                    "В дочернем блоке используйте {{{{{item}.field}}}} — например, {{{{{item}.https_url}}}}."
                ))
                .size(9.0)
                .color(PURPLE),
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
        .wrap(),
    );

    if !step.logs.is_empty() {
        egui::CollapsingHeader::new(format!("Логи блока · {}", step.logs.len()))
            .default_open(matches!(&step.status, StepStatus::Failed))
            .show(ui, |ui| {
                for log in &step.logs {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&log.message)
                                .monospace()
                                .size(8.0)
                                .color(MUTED),
                        )
                        .wrap(),
                    );
                }
            });
    }

    if let Some(output) = &step.output {
        let json = serde_json::to_string_pretty(output)
            .unwrap_or_else(|error| format!("Не удалось вывести контекст: {error}"));
        egui::CollapsingHeader::new("Выходной контекст JSON")
            .default_open(false)
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt(("composer-step-output", &step.step_id))
                    .max_height(240.0)
                    .show(ui, |ui| {
                        Frame::new()
                            .fill(code_surface(dark))
                            .corner_radius(8)
                            .inner_margin(Margin::same(8))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(json).monospace().size(8.0).color(text(dark)),
                                    )
                                    .wrap(),
                                );
                            });
                    });
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
        let mut task = github_repository_composer_task(1);
        let unsupported = Step {
            id: "script".into(),
            name: "Script".into(),
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
            graph: None,
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
        let task = Task {
            id: "branched-scenario".into(),
            name: "Branched scenario".into(),
            description: "Two blocks attached to Start.".into(),
            platform: ppduster::rules::Platform::Macos,
            trust: TrustRequirement::ExternalAllowed,
            scenarios: Vec::new(),
            resolved_scenarios: Vec::new(),
            graph: None,
            steps: vec![
                composer_step(ComposerBlockKind::InspectPath, "inspect-a".into()),
                composer_step(ComposerBlockKind::InspectPath, "inspect-b".into()),
            ],
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
            graph: None,
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
        let mut task = github_repository_composer_task(1);
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
        let mut task = github_repository_composer_task(1);
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
    fn array_selector_recursively_discovers_non_github_arrays() {
        let mut task = github_repository_composer_task(1);
        task.steps[0] = Step {
            id: "run-script".into(),
            name: "Run a script".into(),
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
        let mut task = github_repository_composer_task(1);
        task.steps.push(composer_step(
            ComposerBlockKind::InspectPath,
            "inspect-path".into(),
        ));
        task.steps.push(composer_step(
            ComposerBlockKind::GitInspect,
            "future-git".into(),
        ));

        assert!(composer_condition_fields(&task, 0).is_empty());
        let before_inspect = composer_condition_fields(&task, 1);
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

        let before_future = composer_condition_fields(&task, 2);
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

        let mut task = github_repository_composer_task(1);
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
        assert_eq!(task.steps.len(), 1);
        assert!(matches!(
            task.steps[0].action,
            Action::GithubListRepositories
        ));
        let lines =
            schema_context_lines(&definition_for_action(&task.steps[0].action).output_schema);
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
