//! Versioned workflow graph IR and static structural validation.
//!
//! The graph is intentionally independent from canvas layout. Control-flow
//! nodes own nested graphs, so branch and loop scopes are explicit on the wire
//! and cannot be inferred from editor coordinates or a legacy `parents` map.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::binding::{parse_binding_target, resolve_binding, BindingLimits};
use super::block::definition_for_action;
use super::context::{
    Binding, ContextOrigin, ContextPathSegment, ContextProvenance, ContextScope, ContextStore,
    ContextType, ContextValue, FieldRef, FieldSchema, ObjectSchema, ResolvedSchemaOwned,
    Sensitivity, TemplatePart,
};
use super::expression::{
    check_rule, ExpressionLimits, ExpressionV1, ExpressionValue, ReferenceV1, RuleExprV1,
};
use super::task::{Action, AuthPolicy, ElevationPolicy, Step, StepCondition};

/// Current serialized workflow graph version.
pub const WORKFLOW_GRAPH_VERSION: u32 = 3;

/// Oldest explicit graph wire format that can be migrated without guessing.
/// Version 1 was the legacy linear `steps` representation and is handled only
/// by [`LegacyTaskImporter`].
pub const MIN_MIGRATABLE_WORKFLOW_GRAPH_VERSION: u32 = 2;

// Validation runs before execution and may receive untrusted task packs. Keep
// recursive work and the quadratic/cubic graph algorithms behind a cheap,
// iterative preflight. The total limits permit sizeable composed workflows;
// the local-node cap specifically bounds dominator-set construction.
const GRAPH_MAX_DEPTH: usize = 32;
const GRAPH_MAX_TOTAL_NODES: usize = 4_096;
const GRAPH_MAX_LOCAL_NODES: usize = 512;
const GRAPH_MAX_TOTAL_EDGES: usize = 8_192;
const GRAPH_MAX_ERRORS: usize = 256;
const GRAPH_MAX_VALUE_DEPTH: usize = 32;
const GRAPH_MAX_VALUE_NODES: usize = 4_096;
const GRAPH_MAX_TOTAL_VALUE_NODES: usize = 16_384;
const GRAPH_MAX_TOTAL_EXPRESSION_NODES: usize = 16_384;
const GRAPH_MAX_TOTAL_TEMPLATE_PARTS: usize = 16_384;
const GRAPH_MAX_TOTAL_CONDITION_NODES: usize = 4_096;
const GRAPH_MAX_TOTAL_GRAPHS: usize = 4_096;
const GRAPH_MAX_TOTAL_ENDPOINTS: usize = 16_384;
const GRAPH_MAX_TOTAL_BINDINGS: usize = 16_384;
const GRAPH_MAX_TOTAL_SWITCH_CASES: usize = 4_096;
const GRAPH_MAX_TOTAL_SWITCH_VALUES: usize = 16_384;
const GRAPH_MAX_EXIT_CODES_PER_LIST: usize = 256;
const GRAPH_MAX_TOTAL_EXIT_CODES: usize = 4_096;
const GRAPH_MAX_LITERAL_STRING_BYTES: usize = 1024 * 1024;
const GRAPH_MAX_SERIALIZED_COMPONENT_BYTES: usize = 2 * 1024 * 1024;
const GRAPH_MAX_TOTAL_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

fn workflow_graph_version() -> u32 {
    WORKFLOW_GRAPH_VERSION
}

fn default_concurrency() -> u16 {
    1
}

fn default_item_alias() -> String {
    "item".into()
}

/// An executable, layout-free workflow graph.
///
/// Node IDs are globally unique across this graph and every nested graph. Each
/// nested graph has its own entries and edges; values from a nested graph do
/// not leak into a sibling branch. The owning control node is the stable output
/// boundary for downstream consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraph {
    #[serde(default = "workflow_graph_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub entries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exits: Vec<GraphExit>,
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
}

impl Default for WorkflowGraph {
    fn default() -> Self {
        Self {
            version: WORKFLOW_GRAPH_VERSION,
            id: None,
            entries: Vec::new(),
            exits: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl WorkflowGraph {
    /// Upgrade a previously explicit graph to the canonical v3 wire format.
    ///
    /// v2 and v3 have the same control-flow topology. The v3 boundary makes
    /// graph ownership canonical at the task level and applies recursively to
    /// every lexical subgraph. Unknown versions are rejected rather than
    /// optimistically interpreted.
    pub fn into_v3(mut self) -> Result<Self, WorkflowGraphMigrationError> {
        let mut graph_count = 0usize;
        migrate_graph_version(&mut self, "workflow_graph", 1, &mut graph_count)?;
        Ok(self)
    }

    pub fn to_v3(&self) -> Result<Self, WorkflowGraphMigrationError> {
        self.clone().into_v3()
    }

    /// Validate graph structure and context visibility without executing it.
    ///
    /// All discoverable errors are returned together so the visual editor can
    /// annotate several nodes in one pass.
    pub fn validate(&self) -> Result<(), Vec<GraphValidationError>> {
        let mut validator = GraphValidator::default();
        if validator.preflight(self) {
            validator.collect_ids(self, "graph");
            validator.validate_graph(
                self,
                "graph",
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
            );
        }
        if validator.errors.is_empty() {
            Ok(())
        } else {
            Err(validator.errors)
        }
    }

    /// Convert the declaration order of a legacy v1 step list into an
    /// explicit v3 graph. This function never observes canvas layout or UI
    /// parent metadata.
    ///
    /// Ordinary steps retain their order through `success -> input` edges.
    /// A legacy loop plus its immediately following consumer is migrated only
    /// when membership in the loop body is explicit: either the historical
    /// loop-clone action names the loop, or an ordinary action structurally
    /// references that loop's item scope. This keeps migration fail-closed and
    /// never guesses from adjacency or rewrites positional array references.
    fn import_linear_v1(steps: &[Step]) -> Result<Self, LinearMigrationError> {
        if steps.is_empty() {
            return Err(LinearMigrationError::EmptySteps);
        }
        for step in steps {
            step.validate()
                .map_err(|message| LinearMigrationError::InvalidStep {
                    step: step.id.clone(),
                    message,
                })?;
        }

        let mut nodes = Vec::new();
        let mut top_level_nodes = Vec::new();
        let mut index = 0usize;
        while index < steps.len() {
            let step = &steps[index];
            match &step.action {
                Action::ForEach {
                    source_step,
                    array_path,
                    item,
                    fields,
                } => {
                    let Some(consumer_step) = steps.get(index + 1) else {
                        return Err(unsupported_legacy(
                            step,
                            "for-each is not immediately followed by an explicit loop-item consumer",
                        ));
                    };
                    if !legacy_loop_metadata_is_plain(step) {
                        return Err(unsupported_legacy(
                            step,
                            "loop has conditions, checks, authentication, elevation, or dangerous metadata that has no v2 control-node equivalent",
                        ));
                    }

                    let body_action = match &consumer_step.action {
                        Action::ForEachGitCloneIfMissing {
                            loop_step,
                            repo,
                            dest,
                            branch,
                        } => {
                            if loop_step != &step.id {
                                return Err(unsupported_legacy(
                                    consumer_step,
                                    format!(
                                        "clone references loop {loop_step:?}, expected {:?}",
                                        step.id
                                    ),
                                ));
                            }
                            let (repo_binding, repo_fields) = migrate_item_template(
                                repo,
                                &step.id,
                                item,
                                LegacyTemplateMode::Required,
                            )
                            .map_err(|reason| unsupported_legacy(consumer_step, reason))?;
                            let (dest_binding, dest_fields) = migrate_item_template(
                                dest,
                                &step.id,
                                item,
                                LegacyTemplateMode::Required,
                            )
                            .map_err(|reason| unsupported_legacy(consumer_step, reason))?;
                            let branch_binding = branch
                                .as_deref()
                                .map(|template| {
                                    migrate_item_template(
                                        template,
                                        &step.id,
                                        item,
                                        LegacyTemplateMode::Optional,
                                    )
                                })
                                .transpose()
                                .map_err(|reason| unsupported_legacy(consumer_step, reason))?;

                            let mut referenced_fields = repo_fields;
                            referenced_fields.extend(dest_fields);
                            if let Some((_, branch_fields)) = &branch_binding {
                                referenced_fields.extend(branch_fields.iter().cloned());
                            }
                            prove_legacy_projection(
                                steps,
                                index,
                                source_step,
                                array_path,
                                fields,
                                &referenced_fields,
                            )
                            .map_err(|reason| unsupported_legacy(step, reason))?;

                            let mut converted = consumer_step.clone();
                            converted.bindings.clear();
                            converted.action = Action::GitCloneIfMissing {
                                repo: repo.clone(),
                                dest: dest.clone(),
                                branch: branch.clone(),
                            };
                            let mut bindings = BTreeMap::from([
                                ("repo".into(), repo_binding),
                                ("dest".into(), dest_binding),
                            ]);
                            if let Some((binding, _)) = branch_binding {
                                bindings.insert("branch".into(), binding);
                            }
                            ActionNode {
                                step: converted,
                                bindings,
                            }
                        }
                        Action::ForEach { .. } => {
                            return Err(unsupported_legacy(
                                step,
                                "nested legacy for-each has no explicit body boundary",
                            ));
                        }
                        _ => {
                            if step_condition_references_loop_item(consumer_step, &step.id) {
                                return Err(unsupported_legacy(
                                    consumer_step,
                                    "loop-item conditions in a legacy linear consumer require an explicit v2 graph",
                                ));
                            }
                            let usage = collect_step_loop_item_usage(consumer_step, &step.id);
                            if !usage.found {
                                return Err(unsupported_legacy(
                                    step,
                                    format!(
                                        "following step {:?} does not structurally reference loop item {:?}",
                                        consumer_step.id, step.id
                                    ),
                                ));
                            }
                            if usage.whole_item && !fields.is_empty() {
                                return Err(unsupported_legacy(
                                    consumer_step,
                                    "whole loop item is consumed but the legacy loop projects only selected fields",
                                ));
                            }
                            prove_legacy_projection(
                                steps,
                                index,
                                source_step,
                                array_path,
                                fields,
                                &usage.fields,
                            )
                            .map_err(|reason| unsupported_legacy(step, reason))?;

                            let mut converted = consumer_step.clone();
                            let bindings = std::mem::take(&mut converted.bindings);
                            ActionNode {
                                step: converted,
                                bindings,
                            }
                        }
                    };
                    let body = WorkflowGraph {
                        entries: vec![body_action.step.id.clone()],
                        nodes: vec![GraphNode::Action(Box::new(body_action))],
                        ..WorkflowGraph::default()
                    };
                    let mut collection = FieldRef::step(source_step);
                    for segment in array_path.split('.').filter(|segment| !segment.is_empty()) {
                        collection = collection.field(segment);
                    }
                    nodes.push(GraphNode::ForEach(ForEachNode {
                        id: step.id.clone(),
                        collection: Binding::field(collection),
                        item_alias: item.clone(),
                        index_alias: None,
                        concurrency: 1,
                        on_error: LoopFailurePolicy::Stop,
                        body: Box::new(body),
                    }));
                    top_level_nodes.push((step.id.clone(), EdgePort::Completed));
                    index += 2;
                }
                Action::ForEachGitCloneIfMissing { .. } => {
                    return Err(unsupported_legacy(
                        step,
                        "clone consumer has no immediately preceding compatible for-each",
                    ));
                }
                _ => {
                    let mut embedded_step = step.clone();
                    let bindings = std::mem::take(&mut embedded_step.bindings);
                    nodes.push(GraphNode::Action(Box::new(ActionNode {
                        step: embedded_step,
                        bindings,
                    })));
                    top_level_nodes.push((step.id.clone(), EdgePort::Success));
                    index += 1;
                }
            }
        }

        let entries = top_level_nodes
            .first()
            .map(|(id, _)| id.clone())
            .into_iter()
            .collect();
        let edges = top_level_nodes
            .windows(2)
            .map(|pair| GraphEdge::new(&pair[0].0, pair[0].1.clone(), &pair[1].0))
            .collect();
        let graph = WorkflowGraph {
            entries,
            nodes,
            edges,
            ..WorkflowGraph::default()
        };
        graph
            .validate()
            .map_err(|errors| LinearMigrationError::InvalidGeneratedGraph { errors })?;
        Ok(graph)
    }
}

/// Explicit compatibility boundary for the removed linear task format.
///
/// New editors must construct [`WorkflowGraph`] directly. Keeping legacy
/// lowering behind this named importer prevents canvas coordinates, parent
/// maps, or declaration adjacency from becoming implicit graph semantics.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LegacyTaskImporter;

impl LegacyTaskImporter {
    pub(crate) fn import_steps(steps: &[Step]) -> Result<WorkflowGraph, LinearMigrationError> {
        WorkflowGraph::import_linear_v1(steps)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowGraphMigrationError {
    UnsupportedVersion {
        path: String,
        found: u32,
        minimum: u32,
        current: u32,
    },
    ResourceLimitExceeded {
        path: String,
        resource: &'static str,
        limit: usize,
    },
}

impl fmt::Display for WorkflowGraphMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion {
                path,
                found,
                minimum,
                current,
            } => write!(
                formatter,
                "{path} uses unsupported workflow graph version {found}; migratable versions are {minimum}..={current}"
            ),
            Self::ResourceLimitExceeded {
                path,
                resource,
                limit,
            } => write!(
                formatter,
                "{path} exceeds workflow graph migration {resource} limit {limit}"
            ),
        }
    }
}

impl std::error::Error for WorkflowGraphMigrationError {}

fn migrate_graph_version(
    graph: &mut WorkflowGraph,
    path: &str,
    depth: usize,
    graph_count: &mut usize,
) -> Result<(), WorkflowGraphMigrationError> {
    if depth > GRAPH_MAX_DEPTH {
        return Err(WorkflowGraphMigrationError::ResourceLimitExceeded {
            path: path.into(),
            resource: "nesting depth",
            limit: GRAPH_MAX_DEPTH,
        });
    }
    *graph_count = graph_count.saturating_add(1);
    if *graph_count > GRAPH_MAX_TOTAL_GRAPHS {
        return Err(WorkflowGraphMigrationError::ResourceLimitExceeded {
            path: path.into(),
            resource: "nested graph count",
            limit: GRAPH_MAX_TOTAL_GRAPHS,
        });
    }
    if !(MIN_MIGRATABLE_WORKFLOW_GRAPH_VERSION..=WORKFLOW_GRAPH_VERSION).contains(&graph.version) {
        return Err(WorkflowGraphMigrationError::UnsupportedVersion {
            path: path.into(),
            found: graph.version,
            minimum: MIN_MIGRATABLE_WORKFLOW_GRAPH_VERSION,
            current: WORKFLOW_GRAPH_VERSION,
        });
    }
    graph.version = WORKFLOW_GRAPH_VERSION;

    for (index, node) in graph.nodes.iter_mut().enumerate() {
        let node_path = format!("{path}.nodes[{index}]");
        match node {
            GraphNode::ForEach(node) => migrate_graph_version(
                &mut node.body,
                &format!("{node_path}.body"),
                depth + 1,
                graph_count,
            )?,
            GraphNode::If(node) => {
                migrate_graph_version(
                    &mut node.then_graph,
                    &format!("{node_path}.then"),
                    depth + 1,
                    graph_count,
                )?;
                if let Some(graph) = &mut node.else_graph {
                    migrate_graph_version(
                        graph,
                        &format!("{node_path}.else"),
                        depth + 1,
                        graph_count,
                    )?;
                }
            }
            GraphNode::Switch(node) => {
                for (case_index, case) in node.cases.iter_mut().enumerate() {
                    migrate_graph_version(
                        &mut case.graph,
                        &format!("{node_path}.cases[{case_index}].graph"),
                        depth + 1,
                        graph_count,
                    )?;
                }
                if let Some(graph) = &mut node.default {
                    migrate_graph_version(
                        graph,
                        &format!("{node_path}.default"),
                        depth + 1,
                        graph_count,
                    )?;
                }
            }
            GraphNode::Action(_) | GraphNode::Join(_) => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinearMigrationError {
    EmptySteps,
    InvalidStep { step: String, message: String },
    UnsupportedLegacyControl { step: String, reason: String },
    InvalidGeneratedGraph { errors: Vec<GraphValidationError> },
}

impl fmt::Display for LinearMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySteps => write!(formatter, "cannot migrate an empty v1 step list"),
            Self::InvalidStep { step, message } => {
                write!(formatter, "legacy step {step:?} is invalid: {message}")
            }
            Self::UnsupportedLegacyControl { step, reason } => write!(
                formatter,
                "legacy control step {step:?} cannot be migrated safely: {reason}"
            ),
            Self::InvalidGeneratedGraph { errors } => write!(
                formatter,
                "migrated graph is invalid: {}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

impl std::error::Error for LinearMigrationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyTemplateMode {
    Required,
    Optional,
}

fn unsupported_legacy(step: &Step, reason: impl Into<String>) -> LinearMigrationError {
    LinearMigrationError::UnsupportedLegacyControl {
        step: step.id.clone(),
        reason: reason.into(),
    }
}

fn legacy_loop_metadata_is_plain(step: &Step) -> bool {
    matches!(step.auth, AuthPolicy::None)
        && step.check.is_none()
        && !step.dangerous
        && matches!(step.allow_elevation, ElevationPolicy::Forbidden)
        && step.when.is_none()
        && step.require.is_none()
}

#[derive(Default)]
struct LoopItemUsage {
    found: bool,
    whole_item: bool,
    fields: BTreeSet<String>,
}

fn collect_step_loop_item_usage(step: &Step, loop_id: &str) -> LoopItemUsage {
    let mut usage = LoopItemUsage::default();
    let mut record = |field: &FieldRef| {
        let ContextScope::LoopItem { step_id } = &field.scope else {
            return;
        };
        if step_id != loop_id {
            return;
        }
        usage.found = true;
        match field.segments.first() {
            Some(ContextPathSegment::Field { name }) => {
                usage.fields.insert(name.clone());
            }
            Some(ContextPathSegment::Index { .. }) | None => usage.whole_item = true,
        }
    };

    for binding in step.bindings.values() {
        visit_binding_fields(binding, &mut record);
    }
    usage
}

fn step_condition_references_loop_item(step: &Step, loop_id: &str) -> bool {
    let mut found = false;
    let mut record = |field: &FieldRef| {
        found |= matches!(
            &field.scope,
            ContextScope::LoopItem { step_id } if step_id == loop_id
        );
    };
    for condition in [step.when.as_ref(), step.require.as_ref()]
        .into_iter()
        .flatten()
    {
        visit_condition_fields(condition, &mut record);
    }
    found
}

fn visit_binding_fields(binding: &Binding, visitor: &mut impl FnMut(&FieldRef)) {
    match binding {
        Binding::Field { field } => visitor(field),
        Binding::Interpolated { parts } => {
            for part in parts {
                if let TemplatePart::Field { field } = part {
                    visitor(field);
                }
            }
        }
        Binding::Literal { .. } | Binding::Template { .. } => {}
    }
}

fn visit_condition_fields(condition: &StepCondition, visitor: &mut impl FnMut(&FieldRef)) {
    match condition {
        StepCondition::Expression { rule, .. } => rule.visit_context_references(visitor),
        StepCondition::All { conditions } | StepCondition::Any { conditions } => {
            for condition in conditions {
                visit_condition_fields(condition, visitor);
            }
        }
        StepCondition::Not { condition } => visit_condition_fields(condition, visitor),
        StepCondition::ExitCode { .. } | StepCondition::Path { .. } => {}
    }
}

fn migrate_item_template(
    template: &str,
    loop_id: &str,
    item_alias: &str,
    mode: LegacyTemplateMode,
) -> Result<(Binding, BTreeSet<String>), String> {
    if matches!(mode, LegacyTemplateMode::Optional)
        && template.trim() != template
        && template
            .trim()
            .strip_prefix("{{")
            .and_then(|value| value.strip_suffix("}}"))
            .is_some()
    {
        return Err(
            "optional whole-field template has surrounding whitespace and cannot preserve null semantics"
                .into(),
        );
    }

    let mut parts = Vec::new();
    let mut fields = BTreeSet::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            parts.push(TemplatePart::literal(&rest[..start]));
        }
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("}}") else {
            return Err(format!("unclosed legacy template {template:?}"));
        };
        let expression = after_start[..end].trim();
        let mut segments = expression.split('.');
        if segments.next() != Some(item_alias) {
            return Err(format!(
                "template expression {expression:?} must start with loop item {item_alias:?}"
            ));
        }
        let path = segments.collect::<Vec<_>>();
        if path.is_empty() || path.iter().any(|segment| segment.is_empty()) {
            return Err(format!(
                "template expression {expression:?} must select a non-empty item field path"
            ));
        }
        fields.insert(path[0].to_string());
        let mut field = FieldRef::loop_item(loop_id);
        for segment in path {
            field = field.field(segment);
        }
        parts.push(TemplatePart::field(field));
        rest = &after_start[end + 2..];
    }
    if !rest.is_empty() {
        parts.push(TemplatePart::literal(rest));
    }
    if fields.is_empty() {
        return Ok((Binding::literal(template), fields));
    }
    let binding = if parts.len() == 1 {
        match parts.pop().expect("one template part") {
            TemplatePart::Field { field } => Binding::field(field),
            TemplatePart::Literal { value } => Binding::literal(value),
        }
    } else {
        Binding::interpolated(parts)
    };
    Ok((binding, fields))
}

fn prove_legacy_projection(
    steps: &[Step],
    loop_index: usize,
    source_step: &str,
    array_path: &str,
    projected_fields: &[String],
    referenced_fields: &BTreeSet<String>,
) -> Result<(), String> {
    if projected_fields.is_empty() {
        return Ok(());
    }
    let source = steps[..loop_index]
        .iter()
        .find(|step| step.id == source_step)
        .ok_or_else(|| format!("source step {source_step:?} is not earlier than the loop"))?;
    if !matches!(source.action, Action::GithubListRepositories)
        || array_path != "github.repositories"
    {
        return Err(
            "a non-empty v1 field projection is only proven for github.repositories output".into(),
        );
    }
    let known = BTreeSet::from([
        "id",
        "owner",
        "name",
        "full_name",
        "https_url",
        "ssh_url",
        "default_branch",
        "private",
        "archived",
    ]);
    for field in projected_fields {
        if !known.contains(field.as_str()) {
            return Err(format!(
                "projected GitHub repository field {field:?} is not guaranteed"
            ));
        }
    }
    let selected = projected_fields
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if let Some(field) = referenced_fields
        .iter()
        .find(|field| !selected.contains(field.as_str()))
    {
        return Err(format!(
            "template references field {field:?} omitted by the v1 projection"
        ));
    }
    Ok(())
}

/// One executable or control-flow node.
///
/// Adjacent tagging keeps the payload of every node independently extensible
/// while preserving an unambiguous `kind` discriminator in YAML and JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "config",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum GraphNode {
    Action(Box<ActionNode>),
    ForEach(ForEachNode),
    If(IfNode),
    Switch(SwitchNode),
    Join(JoinNode),
}

impl GraphNode {
    pub fn id(&self) -> &str {
        match self {
            Self::Action(node) => &node.step.id,
            Self::ForEach(node) => &node.id,
            Self::If(node) => &node.id,
            Self::Switch(node) => &node.id,
            Self::Join(node) => &node.id,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Action(_) => "action",
            Self::ForEach(_) => "for-each",
            Self::If(_) => "if",
            Self::Switch(_) => "switch",
            Self::Join(_) => "join",
        }
    }

    fn accepts_output_port(&self, port: &EdgePort) -> bool {
        match self {
            Self::Action(_) => matches!(
                port,
                EdgePort::Success | EdgePort::Failure | EdgePort::Always
            ),
            Self::ForEach(_) => matches!(
                port,
                EdgePort::Completed | EdgePort::Empty | EdgePort::Failure
            ),
            Self::If(_) | Self::Switch(_) | Self::Join(_) => {
                matches!(port, EdgePort::Completed | EdgePort::Failure)
            }
        }
    }
}

/// Atomic action plus typed bindings for its declared input fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionNode {
    pub step: Step,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, Binding>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoopFailurePolicy {
    #[default]
    Stop,
    Continue,
}

/// Iterate a collection in an explicit lexical scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForEachNode {
    pub id: String,
    pub collection: Binding,
    #[serde(default = "default_item_alias")]
    pub item_alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_alias: Option<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: u16,
    #[serde(default)]
    pub on_error: LoopFailurePolicy,
    pub body: Box<WorkflowGraph>,
}

/// Select exactly one of two nested branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IfNode {
    pub id: String,
    pub condition: RuleExprV1,
    #[serde(rename = "then")]
    pub then_graph: Box<WorkflowGraph>,
    #[serde(default, rename = "else", skip_serializing_if = "Option::is_none")]
    pub else_graph: Option<Box<WorkflowGraph>>,
}

/// Match one selector against ordered literal cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchNode {
    pub id: String,
    pub selector: Binding,
    #[serde(default)]
    pub cases: Vec<SwitchCase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Box<WorkflowGraph>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchCase {
    pub id: String,
    #[serde(default)]
    pub values: Vec<Value>,
    pub graph: Box<WorkflowGraph>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JoinMode {
    /// Wait for every incoming path.
    #[default]
    All,
    /// Continue after any incoming path completes.
    Any,
    /// Continue after the first successful incoming path.
    FirstSuccessful,
}

/// Merge two or more paths in the current lexical graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinNode {
    pub id: String,
    #[serde(default)]
    pub mode: JoinMode,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgePort {
    /// The only valid destination port.
    Input,
    /// Atomic action succeeded.
    Success,
    /// Node failed.
    Failure,
    /// Action finished regardless of status.
    Always,
    /// Control node completed normally.
    Completed,
    /// A `for-each` collection contained no items.
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeEndpoint {
    pub node: String,
    pub port: EdgePort,
}

impl EdgeEndpoint {
    pub fn input(node: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            port: EdgePort::Input,
        }
    }

    pub fn output(node: impl Into<String>, port: EdgePort) -> Self {
        Self {
            node: node.into(),
            port,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEdge {
    pub from: EdgeEndpoint,
    pub to: EdgeEndpoint,
}

impl GraphEdge {
    pub fn new(from: impl Into<String>, port: EdgePort, to: impl Into<String>) -> Self {
        Self {
            from: EdgeEndpoint::output(from, port),
            to: EdgeEndpoint::input(to),
        }
    }
}

/// Named result of a nested or root graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphExit {
    pub name: String,
    pub from: EdgeEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphValidationError {
    pub path: String,
    pub kind: GraphValidationErrorKind,
}

impl fmt::Display for GraphValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.kind)
    }
}

impl std::error::Error for GraphValidationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphValidationErrorKind {
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    EmptyGraphId,
    EmptyGraph,
    EmptyNodeId,
    ReservedNodeId {
        id: String,
    },
    DuplicateNodeId {
        id: String,
        first_path: String,
    },
    EmptyEntries,
    DuplicateEntry {
        node: String,
    },
    UnknownEntry {
        node: String,
    },
    EntryHasIncoming {
        node: String,
    },
    UnknownEndpoint {
        node: String,
    },
    InvalidSourcePort {
        node: String,
        kind: String,
        port: EdgePort,
    },
    InvalidTargetPort {
        node: String,
        port: EdgePort,
    },
    DuplicateEdge,
    Cycle {
        nodes: Vec<String>,
    },
    UnreachableNode {
        node: String,
    },
    EmptyExitName,
    DuplicateExitName {
        name: String,
    },
    UnlistedTerminal {
        node: String,
    },
    JoinNeedsMultipleInputs {
        node: String,
        found: usize,
    },
    ResourceLimitExceeded {
        resource: String,
        found: usize,
        limit: usize,
    },
    ErrorLimitReached {
        limit: usize,
    },
    InvalidAction {
        node: String,
        message: String,
    },
    LegacyControlAction {
        node: String,
    },
    EmptyBindingName {
        node: String,
    },
    DuplicateBindingTarget {
        node: String,
        first: String,
        duplicate: String,
    },
    UnknownBindingField {
        node: String,
        field: String,
    },
    UnknownContextField {
        consumer: String,
        producer: String,
        field: String,
    },
    OutputSchemaUnavailable {
        consumer: String,
        producer: String,
    },
    BindingTypeMismatch {
        node: String,
        field: String,
        expected: ContextType,
        actual: ContextType,
    },
    BindingMayBeMissing {
        node: String,
        field: String,
    },
    BindingMayBeNull {
        node: String,
        field: String,
    },
    InvalidBindingValue {
        node: String,
        field: String,
        message: String,
    },
    SecretBindingFlow {
        node: String,
        field: String,
    },
    ForEachCollectionNotArray {
        node: String,
        actual: Option<ContextType>,
    },
    InterpolatedFieldNotScalar {
        consumer: String,
        field: String,
        actual: ContextType,
    },
    InvalidAlias {
        node: String,
        alias: String,
    },
    ShadowedAlias {
        node: String,
        alias: String,
    },
    DuplicateAlias {
        node: String,
        alias: String,
    },
    InvalidConcurrency {
        node: String,
    },
    EmptySwitchCases {
        node: String,
    },
    EmptySwitchCaseId {
        node: String,
    },
    DuplicateSwitchCaseId {
        node: String,
        case: String,
    },
    EmptySwitchCaseValues {
        node: String,
        case: String,
    },
    DuplicateSwitchValue {
        node: String,
        value: String,
    },
    SwitchCaseTypeMismatch {
        node: String,
        case: String,
        selector: ContextType,
        value: ContextType,
    },
    InvalidExpression {
        node: String,
        code: String,
        location: String,
        message: String,
    },
    ExitCodeSourceNotRunScript {
        consumer: String,
        producer: String,
    },
    ExitCodeNotSuccessful {
        consumer: String,
        producer: String,
        code: u32,
    },
    UnknownContextStep {
        consumer: String,
        producer: String,
    },
    ContextNotVisible {
        consumer: String,
        producer: String,
    },
    LoopContextNotVisible {
        consumer: String,
        loop_node: String,
    },
    LocalNotVisible {
        consumer: String,
        binding: String,
    },
    InvalidLocalPath {
        consumer: String,
        binding: String,
    },
}

impl fmt::Display for GraphValidationErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GraphValidationErrorKind as Kind;
        match self {
            Kind::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported graph version {found}; supported version is {supported}"
            ),
            Kind::EmptyGraphId => write!(formatter, "graph id must not be empty"),
            Kind::EmptyGraph => write!(formatter, "graph must contain at least one node"),
            Kind::EmptyNodeId => write!(formatter, "node id must not be empty"),
            Kind::ReservedNodeId { id } => write!(
                formatter,
                "node id {id:?} uses the reserved suffix \"::index\""
            ),
            Kind::DuplicateNodeId { id, first_path } => {
                write!(
                    formatter,
                    "duplicate node id {id:?}; first declared at {first_path}"
                )
            }
            Kind::EmptyEntries => write!(formatter, "graph must declare at least one entry"),
            Kind::DuplicateEntry { node } => write!(formatter, "duplicate entry {node:?}"),
            Kind::UnknownEntry { node } => write!(formatter, "unknown entry node {node:?}"),
            Kind::EntryHasIncoming { node } => {
                write!(formatter, "entry node {node:?} has an incoming edge")
            }
            Kind::UnknownEndpoint { node } => write!(formatter, "unknown edge endpoint {node:?}"),
            Kind::InvalidSourcePort { node, kind, port } => write!(
                formatter,
                "node {node:?} ({kind}) cannot emit from port {port:?}"
            ),
            Kind::InvalidTargetPort { node, port } => write!(
                formatter,
                "node {node:?} cannot receive on port {port:?}; expected input"
            ),
            Kind::DuplicateEdge => write!(formatter, "duplicate graph edge"),
            Kind::Cycle { nodes } => write!(formatter, "graph contains a cycle through {nodes:?}"),
            Kind::UnreachableNode { node } => write!(formatter, "node {node:?} is unreachable"),
            Kind::EmptyExitName => write!(formatter, "graph exit name must not be empty"),
            Kind::DuplicateExitName { name } => write!(formatter, "duplicate graph exit {name:?}"),
            Kind::UnlistedTerminal { node } => write!(
                formatter,
                "terminal node {node:?} is not represented by a graph exit"
            ),
            Kind::JoinNeedsMultipleInputs { node, found } => write!(
                formatter,
                "join {node:?} needs at least two incoming paths, found {found}"
            ),
            Kind::ResourceLimitExceeded {
                resource,
                found,
                limit,
            } => write!(
                formatter,
                "workflow graph {resource} exceeds limit {limit} (found at least {found})"
            ),
            Kind::ErrorLimitReached { limit } => write!(
                formatter,
                "workflow graph validation stopped after {limit} errors"
            ),
            Kind::InvalidAction { node, message } => {
                write!(formatter, "action node {node:?} is invalid: {message}")
            }
            Kind::LegacyControlAction { node } => write!(
                formatter,
                "action node {node:?} uses a v1 loop action; use a graph for-each node"
            ),
            Kind::EmptyBindingName { node } => {
                write!(
                    formatter,
                    "action node {node:?} contains an empty binding name"
                )
            }
            Kind::DuplicateBindingTarget {
                node,
                first,
                duplicate,
            } => write!(
                formatter,
                "action node {node:?} binds the same input through {first:?} and {duplicate:?}"
            ),
            Kind::UnknownBindingField { node, field } => write!(
                formatter,
                "action node {node:?} has no declared input field {field:?}"
            ),
            Kind::UnknownContextField {
                consumer,
                producer,
                field,
            } => write!(
                formatter,
                "node {consumer:?} references unknown output field {field:?} on {producer:?}"
            ),
            Kind::OutputSchemaUnavailable { consumer, producer } => write!(
                formatter,
                "node {consumer:?} cannot statically bind output of {producer:?}: no output schema"
            ),
            Kind::BindingTypeMismatch {
                node,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "binding {node:?}.{field} expects {expected:?}, got {actual:?}"
            ),
            Kind::BindingMayBeMissing { node, field } => write!(
                formatter,
                "binding {node:?}.{field} uses an optional source field"
            ),
            Kind::BindingMayBeNull { node, field } => write!(
                formatter,
                "binding {node:?}.{field} uses a nullable source field"
            ),
            Kind::InvalidBindingValue {
                node,
                field,
                message,
            } => write!(
                formatter,
                "binding {node:?}.{field} violates its declared input contract: {message}"
            ),
            Kind::SecretBindingFlow { node, field } => write!(
                formatter,
                "binding {node:?}.{field} sends secret context to a non-secret input"
            ),
            Kind::ForEachCollectionNotArray { node, actual } => write!(
                formatter,
                "for-each {node:?} collection must be an array, got {actual:?}"
            ),
            Kind::InterpolatedFieldNotScalar {
                consumer,
                field,
                actual,
            } => write!(
                formatter,
                "interpolated field {field:?} in {consumer:?} must be scalar, got {actual:?}"
            ),
            Kind::InvalidAlias { node, alias } => {
                write!(formatter, "node {node:?} contains invalid alias {alias:?}")
            }
            Kind::ShadowedAlias { node, alias } => {
                write!(formatter, "node {node:?} shadows visible alias {alias:?}")
            }
            Kind::DuplicateAlias { node, alias } => {
                write!(formatter, "node {node:?} declares alias {alias:?} twice")
            }
            Kind::InvalidConcurrency { node } => {
                write!(
                    formatter,
                    "for-each {node:?} concurrency must be greater than zero"
                )
            }
            Kind::EmptySwitchCases { node } => {
                write!(formatter, "switch {node:?} must define at least one case")
            }
            Kind::EmptySwitchCaseId { node } => {
                write!(formatter, "switch {node:?} contains an empty case id")
            }
            Kind::DuplicateSwitchCaseId { node, case } => {
                write!(
                    formatter,
                    "switch {node:?} contains duplicate case {case:?}"
                )
            }
            Kind::EmptySwitchCaseValues { node, case } => write!(
                formatter,
                "switch {node:?} case {case:?} must match at least one value"
            ),
            Kind::DuplicateSwitchValue { node, value } => write!(
                formatter,
                "switch {node:?} matches value {value} in more than one case"
            ),
            Kind::SwitchCaseTypeMismatch {
                node,
                case,
                selector,
                value,
            } => write!(
                formatter,
                "switch {node:?} case {case:?} compares selector {selector:?} with incompatible value {value:?}"
            ),
            Kind::InvalidExpression {
                node,
                code,
                location,
                message,
            } => write!(
                formatter,
                "node {node:?} has invalid expression ({code} at {location}): {message}"
            ),
            Kind::ExitCodeSourceNotRunScript { consumer, producer } => write!(
                formatter,
                "exit-code condition on {consumer:?} references {producer:?}, which is not a run-script action"
            ),
            Kind::ExitCodeNotSuccessful {
                consumer,
                producer,
                code,
            } => write!(
                formatter,
                "exit-code condition on {consumer:?} uses code {code}, which is not in {producer:?}.success_exit_codes"
            ),
            Kind::UnknownContextStep { consumer, producer } => write!(
                formatter,
                "node {consumer:?} references unknown context step {producer:?}"
            ),
            Kind::ContextNotVisible { consumer, producer } => write!(
                formatter,
                "context from {producer:?} does not dominate consumer {consumer:?}"
            ),
            Kind::LoopContextNotVisible {
                consumer,
                loop_node,
            } => write!(
                formatter,
                "loop context {loop_node:?} is not visible to {consumer:?}"
            ),
            Kind::LocalNotVisible { consumer, binding } => write!(
                formatter,
                "local binding {binding:?} is not visible to {consumer:?}"
            ),
            Kind::InvalidLocalPath { consumer, binding } => write!(
                formatter,
                "local binding {binding:?} contains an empty path segment in {consumer:?}"
            ),
        }
    }
}

#[derive(Default)]
struct GraphValidator {
    global_ids: BTreeMap<String, String>,
    global_output_schemas: BTreeMap<String, ObjectSchema>,
    global_script_exit_codes: BTreeMap<String, Option<BTreeSet<u32>>>,
    errors: Vec<GraphValidationError>,
    error_limit_reached: bool,
}

impl GraphValidator {
    fn error(&mut self, path: impl Into<String>, kind: GraphValidationErrorKind) {
        if self.error_limit_reached {
            return;
        }
        if self.errors.len() == GRAPH_MAX_ERRORS.saturating_sub(1) {
            self.errors.push(GraphValidationError {
                path: "graph".into(),
                kind: GraphValidationErrorKind::ErrorLimitReached {
                    limit: GRAPH_MAX_ERRORS,
                },
            });
            self.error_limit_reached = true;
            return;
        }
        self.errors.push(GraphValidationError {
            path: path.into(),
            kind,
        });
    }

    /// Cheap iterative pass performed before any recursive descent, cloning of
    /// expression trees, or dominator allocation.
    fn preflight(&mut self, root: &WorkflowGraph) -> bool {
        let mut valid = true;
        let mut budget = GraphPreflightBudget::default();
        let mut stack = vec![(root, String::from("graph"), 1usize)];

        while let Some((graph, path, depth)) = stack.pop() {
            budget.graphs = budget.graphs.saturating_add(1);
            if budget.graphs > GRAPH_MAX_TOTAL_GRAPHS {
                self.resource_limit(
                    &path,
                    "nested graph count",
                    budget.graphs,
                    GRAPH_MAX_TOTAL_GRAPHS,
                );
                valid = false;
                break;
            }
            if depth > GRAPH_MAX_DEPTH {
                self.resource_limit(&path, "nesting depth", depth, GRAPH_MAX_DEPTH);
                valid = false;
                continue;
            }
            if graph.nodes.len() > GRAPH_MAX_LOCAL_NODES {
                self.resource_limit(
                    &path,
                    "nodes in one lexical graph",
                    graph.nodes.len(),
                    GRAPH_MAX_LOCAL_NODES,
                );
                valid = false;
            }
            budget.nodes = budget.nodes.saturating_add(graph.nodes.len());
            if budget.nodes > GRAPH_MAX_TOTAL_NODES {
                self.resource_limit(&path, "total nodes", budget.nodes, GRAPH_MAX_TOTAL_NODES);
                valid = false;
                break;
            }
            budget.edges = budget.edges.saturating_add(graph.edges.len());
            if budget.edges > GRAPH_MAX_TOTAL_EDGES {
                self.resource_limit(&path, "total edges", budget.edges, GRAPH_MAX_TOTAL_EDGES);
                valid = false;
                break;
            }
            budget.endpoints = budget
                .endpoints
                .saturating_add(graph.entries.len())
                .saturating_add(graph.exits.len());
            if budget.endpoints > GRAPH_MAX_TOTAL_ENDPOINTS {
                self.resource_limit(
                    &path,
                    "total entries and exits",
                    budget.endpoints,
                    GRAPH_MAX_TOTAL_ENDPOINTS,
                );
                valid = false;
                break;
            }

            if let Some(id) = &graph.id {
                valid &= self.preflight_string(id, &format!("{path}.id"), &mut budget);
            }
            for (entry_index, entry) in graph.entries.iter().enumerate() {
                valid &= self.preflight_string(
                    entry,
                    &format!("{path}.entries[{entry_index}]"),
                    &mut budget,
                );
            }
            for (exit_index, exit) in graph.exits.iter().enumerate() {
                valid &= self.preflight_string(
                    &exit.name,
                    &format!("{path}.exits[{exit_index}].name"),
                    &mut budget,
                );
                valid &= self.preflight_string(
                    &exit.from.node,
                    &format!("{path}.exits[{exit_index}].from.node"),
                    &mut budget,
                );
            }
            for (edge_index, edge) in graph.edges.iter().enumerate() {
                valid &= self.preflight_string(
                    &edge.from.node,
                    &format!("{path}.edges[{edge_index}].from.node"),
                    &mut budget,
                );
                valid &= self.preflight_string(
                    &edge.to.node,
                    &format!("{path}.edges[{edge_index}].to.node"),
                    &mut budget,
                );
            }

            for (index, node) in graph.nodes.iter().enumerate() {
                let node_path = format!("{path}.nodes[{index}]");
                valid &= self.preflight_string(node.id(), &format!("{node_path}.id"), &mut budget);
                match node {
                    GraphNode::Action(action) => {
                        valid &= self.preflight_string(
                            &action.step.name,
                            &format!("{node_path}.name"),
                            &mut budget,
                        );
                        if let Some(check) = &action.step.check {
                            valid &= self.preflight_serialized_component(
                                check,
                                &format!("{node_path}.check"),
                                &mut budget,
                            );
                        }
                        valid &= self.preflight_serialized_component(
                            &action.step.action,
                            &format!("{node_path}.action"),
                            &mut budget,
                        );
                        if let Action::RunScript {
                            success_exit_codes, ..
                        } = &action.step.action
                        {
                            valid &= self.preflight_exit_codes(
                                success_exit_codes,
                                &format!("{node_path}.action.success_exit_codes"),
                                &mut budget,
                            );
                        }
                        let binding_limits = BindingLimits::default();
                        if action.bindings.len() > binding_limits.max_bindings {
                            self.resource_limit(
                                &node_path,
                                "bindings on one action",
                                action.bindings.len(),
                                binding_limits.max_bindings,
                            );
                            valid = false;
                            continue;
                        }
                        budget.bindings = budget.bindings.saturating_add(action.bindings.len());
                        if budget.bindings > GRAPH_MAX_TOTAL_BINDINGS {
                            self.resource_limit(
                                &node_path,
                                "total action bindings",
                                budget.bindings,
                                GRAPH_MAX_TOTAL_BINDINGS,
                            );
                            valid = false;
                        } else {
                            for (name, binding) in &action.bindings {
                                valid &= self.preflight_string(
                                    name,
                                    &format!("{node_path}.bindings.target"),
                                    &mut budget,
                                );
                                valid &= self.preflight_binding(
                                    binding,
                                    &format!("{node_path}.bindings[{name:?}]"),
                                    &mut budget,
                                );
                            }
                        }
                        for (condition_name, condition) in [
                            ("when", action.step.when.as_ref()),
                            ("require", action.step.require.as_ref()),
                        ] {
                            if let Some(condition) = condition {
                                valid &= self.preflight_condition(
                                    condition,
                                    &format!("{node_path}.{condition_name}"),
                                    &mut budget,
                                );
                            }
                        }
                    }
                    GraphNode::ForEach(control) => {
                        valid &= self.preflight_string(
                            &control.item_alias,
                            &format!("{node_path}.item_alias"),
                            &mut budget,
                        );
                        if let Some(index_alias) = &control.index_alias {
                            valid &= self.preflight_string(
                                index_alias,
                                &format!("{node_path}.index_alias"),
                                &mut budget,
                            );
                        }
                        valid &= self.preflight_binding(
                            &control.collection,
                            &format!("{node_path}.collection"),
                            &mut budget,
                        );
                        stack.push((
                            &control.body,
                            format!("{node_path}.body"),
                            depth.saturating_add(1),
                        ));
                    }
                    GraphNode::If(control) => {
                        valid &= self.preflight_expression(
                            &control.condition,
                            &format!("{node_path}.condition"),
                            &mut budget,
                        );
                        stack.push((
                            &control.then_graph,
                            format!("{node_path}.then"),
                            depth.saturating_add(1),
                        ));
                        if let Some(graph) = &control.else_graph {
                            stack.push((
                                graph,
                                format!("{node_path}.else"),
                                depth.saturating_add(1),
                            ));
                        }
                    }
                    GraphNode::Switch(control) => {
                        valid &= self.preflight_binding(
                            &control.selector,
                            &format!("{node_path}.selector"),
                            &mut budget,
                        );
                        budget.switch_cases =
                            budget.switch_cases.saturating_add(control.cases.len());
                        if budget.switch_cases > GRAPH_MAX_TOTAL_SWITCH_CASES {
                            self.resource_limit(
                                &node_path,
                                "total switch cases",
                                budget.switch_cases,
                                GRAPH_MAX_TOTAL_SWITCH_CASES,
                            );
                            valid = false;
                        } else {
                            for (case_index, case) in control.cases.iter().enumerate() {
                                valid &= self.preflight_string(
                                    &case.id,
                                    &format!("{node_path}.cases[{case_index}].id"),
                                    &mut budget,
                                );
                                budget.switch_values =
                                    budget.switch_values.saturating_add(case.values.len());
                                if budget.switch_values > GRAPH_MAX_TOTAL_SWITCH_VALUES {
                                    self.resource_limit(
                                        &node_path,
                                        "total switch case values",
                                        budget.switch_values,
                                        GRAPH_MAX_TOTAL_SWITCH_VALUES,
                                    );
                                    valid = false;
                                    break;
                                }
                                for (value_index, value) in case.values.iter().enumerate() {
                                    valid &= self.preflight_json_value(
                                        value,
                                        &format!(
                                            "{node_path}.cases[{case_index}].values[{value_index}]"
                                        ),
                                        &mut budget,
                                    );
                                }
                                stack.push((
                                    &case.graph,
                                    format!("{node_path}.cases[{case_index}].graph"),
                                    depth.saturating_add(1),
                                ));
                            }
                        }
                        if let Some(graph) = &control.default {
                            stack.push((
                                graph,
                                format!("{node_path}.default"),
                                depth.saturating_add(1),
                            ));
                        }
                    }
                    GraphNode::Join(_) => {}
                }
            }
        }
        valid
    }

    fn resource_limit(&mut self, path: &str, resource: &str, found: usize, limit: usize) {
        self.error(
            path,
            GraphValidationErrorKind::ResourceLimitExceeded {
                resource: resource.into(),
                found,
                limit,
            },
        );
    }

    fn preflight_payload_bytes(
        &mut self,
        bytes: usize,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        if budget.payload_bytes > GRAPH_MAX_TOTAL_PAYLOAD_BYTES {
            return false;
        }
        budget.payload_bytes = budget.payload_bytes.saturating_add(bytes);
        if budget.payload_bytes > GRAPH_MAX_TOTAL_PAYLOAD_BYTES {
            self.resource_limit(
                path,
                "total graph payload bytes",
                budget.payload_bytes,
                GRAPH_MAX_TOTAL_PAYLOAD_BYTES,
            );
            false
        } else {
            true
        }
    }

    fn preflight_string(
        &mut self,
        value: &str,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        if value.len() > GRAPH_MAX_LITERAL_STRING_BYTES {
            self.resource_limit(
                path,
                "single graph string bytes",
                value.len(),
                GRAPH_MAX_LITERAL_STRING_BYTES,
            );
            return false;
        }
        self.preflight_payload_bytes(value.len(), path, budget)
    }

    fn preflight_serialized_component<T: Serialize>(
        &mut self,
        value: &T,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        match serialized_payload_bytes(value) {
            Ok(bytes) => self.preflight_payload_bytes(bytes, path, budget),
            Err(limit) => {
                self.resource_limit(path, limit.resource, limit.found, limit.limit);
                false
            }
        }
    }

    fn preflight_exit_codes(
        &mut self,
        codes: &[u32],
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        if codes.len() > GRAPH_MAX_EXIT_CODES_PER_LIST {
            self.resource_limit(
                path,
                "exit codes in one list",
                codes.len(),
                GRAPH_MAX_EXIT_CODES_PER_LIST,
            );
            return false;
        }
        budget.exit_codes = budget.exit_codes.saturating_add(codes.len());
        if budget.exit_codes > GRAPH_MAX_TOTAL_EXIT_CODES {
            self.resource_limit(
                path,
                "total exit codes",
                budget.exit_codes,
                GRAPH_MAX_TOTAL_EXIT_CODES,
            );
            false
        } else {
            true
        }
    }

    fn preflight_condition(
        &mut self,
        root: &StepCondition,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        const MAX_DEPTH: usize = 32;
        const MAX_NODES: usize = 256;

        let mut valid = true;
        let mut local_nodes = 0usize;
        let mut stack = vec![(root, 1usize)];
        while let Some((condition, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                self.resource_limit(path, "condition depth", depth, MAX_DEPTH);
                return false;
            }
            local_nodes = local_nodes.saturating_add(1);
            budget.condition_nodes = budget.condition_nodes.saturating_add(1);
            if local_nodes > MAX_NODES {
                self.resource_limit(path, "condition nodes", local_nodes, MAX_NODES);
                return false;
            }
            if budget.condition_nodes > GRAPH_MAX_TOTAL_CONDITION_NODES {
                self.resource_limit(
                    path,
                    "total condition nodes",
                    budget.condition_nodes,
                    GRAPH_MAX_TOTAL_CONDITION_NODES,
                );
                return false;
            }
            match condition {
                StepCondition::Expression { rule, .. } => {
                    valid &= self.preflight_expression(rule, path, budget);
                }
                StepCondition::All { conditions } | StepCondition::Any { conditions } => {
                    let prospective = local_nodes
                        .saturating_add(stack.len())
                        .saturating_add(conditions.len());
                    if prospective > MAX_NODES {
                        self.resource_limit(path, "condition nodes", prospective, MAX_NODES);
                        return false;
                    }
                    let total_prospective = budget
                        .condition_nodes
                        .saturating_add(stack.len())
                        .saturating_add(conditions.len());
                    if total_prospective > GRAPH_MAX_TOTAL_CONDITION_NODES {
                        self.resource_limit(
                            path,
                            "total condition nodes",
                            total_prospective,
                            GRAPH_MAX_TOTAL_CONDITION_NODES,
                        );
                        return false;
                    }
                    stack.extend(
                        conditions
                            .iter()
                            .map(|condition| (condition, depth.saturating_add(1))),
                    );
                }
                StepCondition::Not { condition } => {
                    stack.push((condition, depth.saturating_add(1)));
                }
                StepCondition::ExitCode { step, codes } => {
                    valid &= self.preflight_string(step, path, budget);
                    valid &= self.preflight_exit_codes(codes, path, budget);
                }
                StepCondition::Path {
                    path: condition_path,
                    expect,
                } => {
                    valid &= self.preflight_string(condition_path, path, budget);
                    if let Some(sha256) = &expect.sha256 {
                        valid &= self.preflight_string(sha256, path, budget);
                    }
                }
            }
        }
        valid
    }

    fn preflight_expression(
        &mut self,
        expression: &ExpressionV1,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        let limits = ExpressionLimits::default();
        match expression_stats(expression, limits) {
            Ok(stats) => {
                budget.expression_nodes = budget.expression_nodes.saturating_add(stats.nodes);
                if budget.expression_nodes > GRAPH_MAX_TOTAL_EXPRESSION_NODES {
                    self.resource_limit(
                        path,
                        "total expression nodes",
                        budget.expression_nodes,
                        GRAPH_MAX_TOTAL_EXPRESSION_NODES,
                    );
                    false
                } else {
                    self.preflight_payload_bytes(stats.payload_bytes, path, budget)
                }
            }
            Err(limit) => {
                self.resource_limit(path, limit.resource, limit.found, limit.limit);
                false
            }
        }
    }

    fn preflight_binding(
        &mut self,
        binding: &Binding,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        let limits = BindingLimits::default();
        match binding {
            Binding::Literal { value } => self.preflight_json_value(value, path, budget),
            Binding::Field { field } => {
                if field.segments.len() > limits.max_path_segments {
                    self.resource_limit(
                        path,
                        "binding path segments",
                        field.segments.len(),
                        limits.max_path_segments,
                    );
                    false
                } else {
                    match field_ref_payload_bytes(field) {
                        Ok(bytes) => self.preflight_payload_bytes(bytes, path, budget),
                        Err(limit) => {
                            self.resource_limit(path, limit.resource, limit.found, limit.limit);
                            false
                        }
                    }
                }
            }
            Binding::Template { template } => {
                if template.len() > limits.max_rendered_bytes {
                    self.resource_limit(
                        path,
                        "template bytes",
                        template.len(),
                        limits.max_rendered_bytes,
                    );
                    false
                } else {
                    self.preflight_payload_bytes(template.len(), path, budget)
                }
            }
            Binding::Interpolated { parts } => {
                let mut valid = true;
                budget.template_parts = budget.template_parts.saturating_add(parts.len());
                if parts.len() > limits.max_template_parts {
                    self.resource_limit(
                        path,
                        "template parts",
                        parts.len(),
                        limits.max_template_parts,
                    );
                    valid = false;
                }
                if budget.template_parts > GRAPH_MAX_TOTAL_TEMPLATE_PARTS {
                    self.resource_limit(
                        path,
                        "total template parts",
                        budget.template_parts,
                        GRAPH_MAX_TOTAL_TEMPLATE_PARTS,
                    );
                    valid = false;
                }
                let literal_bytes = parts.iter().fold(0usize, |bytes, part| {
                    bytes.saturating_add(match part {
                        TemplatePart::Literal { value } => value.len(),
                        TemplatePart::Field { .. } => 0,
                    })
                });
                if literal_bytes > limits.max_rendered_bytes {
                    self.resource_limit(
                        path,
                        "template literal bytes",
                        literal_bytes,
                        limits.max_rendered_bytes,
                    );
                    valid = false;
                } else {
                    valid &= self.preflight_payload_bytes(literal_bytes, path, budget);
                }
                for part in parts {
                    if let TemplatePart::Field { field } = part {
                        if field.segments.len() > limits.max_path_segments {
                            self.resource_limit(
                                path,
                                "template field path segments",
                                field.segments.len(),
                                limits.max_path_segments,
                            );
                            valid = false;
                        } else {
                            match field_ref_payload_bytes(field) {
                                Ok(bytes) => {
                                    valid &= self.preflight_payload_bytes(bytes, path, budget);
                                }
                                Err(limit) => {
                                    self.resource_limit(
                                        path,
                                        limit.resource,
                                        limit.found,
                                        limit.limit,
                                    );
                                    valid = false;
                                }
                            }
                        }
                    }
                }
                valid
            }
        }
    }

    fn preflight_json_value(
        &mut self,
        value: &Value,
        path: &str,
        budget: &mut GraphPreflightBudget,
    ) -> bool {
        match json_value_stats(value) {
            Ok(stats) => {
                budget.value_nodes = budget.value_nodes.saturating_add(stats.nodes);
                if budget.value_nodes > GRAPH_MAX_TOTAL_VALUE_NODES {
                    self.resource_limit(
                        path,
                        "total literal value nodes",
                        budget.value_nodes,
                        GRAPH_MAX_TOTAL_VALUE_NODES,
                    );
                    false
                } else {
                    self.preflight_payload_bytes(stats.payload_bytes, path, budget)
                }
            }
            Err(limit) => {
                self.resource_limit(path, limit.resource, limit.found, limit.limit);
                false
            }
        }
    }

    fn collect_ids(&mut self, graph: &WorkflowGraph, path: &str) {
        for (index, node) in graph.nodes.iter().enumerate() {
            let node_path = format!("{path}.nodes[{index}]");
            let id = node.id();
            if id.trim().is_empty() {
                self.error(&node_path, GraphValidationErrorKind::EmptyNodeId);
            } else if id.ends_with("::index") {
                self.error(
                    &node_path,
                    GraphValidationErrorKind::ReservedNodeId { id: id.into() },
                );
            } else if let Some(first_path) = self.global_ids.get(id).cloned() {
                self.error(
                    &node_path,
                    GraphValidationErrorKind::DuplicateNodeId {
                        id: id.into(),
                        first_path,
                    },
                );
            } else {
                self.global_ids.insert(id.into(), node_path.clone());
            }

            if let GraphNode::Action(action) = node {
                self.global_output_schemas
                    .entry(id.into())
                    .or_insert_with(|| definition_for_action(&action.step.action).output_schema);
                self.global_script_exit_codes
                    .entry(id.into())
                    .or_insert_with(|| match &action.step.action {
                        Action::RunScript {
                            success_exit_codes, ..
                        } => Some(success_exit_codes.iter().copied().collect()),
                        _ => None,
                    });
            }

            match node {
                GraphNode::ForEach(node) => {
                    self.collect_ids(&node.body, &format!("{node_path}.body"));
                }
                GraphNode::If(node) => {
                    self.collect_ids(&node.then_graph, &format!("{node_path}.then"));
                    if let Some(graph) = &node.else_graph {
                        self.collect_ids(graph, &format!("{node_path}.else"));
                    }
                }
                GraphNode::Switch(node) => {
                    for (case_index, case) in node.cases.iter().enumerate() {
                        self.collect_ids(
                            &case.graph,
                            &format!("{node_path}.cases[{case_index}].graph"),
                        );
                    }
                    if let Some(graph) = &node.default {
                        self.collect_ids(graph, &format!("{node_path}.default"));
                    }
                }
                GraphNode::Action(_) | GraphNode::Join(_) => {}
            }
        }
    }

    fn validate_graph(
        &mut self,
        graph: &WorkflowGraph,
        path: &str,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        active_aliases: &BTreeMap<String, FieldRef>,
    ) {
        if graph.version != WORKFLOW_GRAPH_VERSION {
            self.error(
                path,
                GraphValidationErrorKind::UnsupportedVersion {
                    found: graph.version,
                    supported: WORKFLOW_GRAPH_VERSION,
                },
            );
        }
        if graph.id.as_ref().is_some_and(|id| id.trim().is_empty()) {
            self.error(path, GraphValidationErrorKind::EmptyGraphId);
        }
        if graph.nodes.is_empty() {
            self.error(path, GraphValidationErrorKind::EmptyGraph);
        }
        if graph.entries.is_empty() {
            self.error(path, GraphValidationErrorKind::EmptyEntries);
        }

        let mut nodes = BTreeMap::<String, &GraphNode>::new();
        for node in &graph.nodes {
            if !node.id().trim().is_empty() {
                nodes.entry(node.id().into()).or_insert(node);
            }
        }
        let local_ids = nodes.keys().cloned().collect::<BTreeSet<_>>();
        let mut predecessors = local_ids
            .iter()
            .map(|id| (id.clone(), BTreeSet::<String>::new()))
            .collect::<BTreeMap<_, _>>();
        let mut successors = predecessors.clone();
        let mut incoming_count = local_ids
            .iter()
            .map(|id| (id.clone(), 0usize))
            .collect::<BTreeMap<_, _>>();

        let mut unique_edges = BTreeSet::new();
        for (index, edge) in graph.edges.iter().enumerate() {
            let edge_path = format!("{path}.edges[{index}]");
            if !unique_edges.insert(edge.clone()) {
                self.error(&edge_path, GraphValidationErrorKind::DuplicateEdge);
            }

            let source = nodes.get(&edge.from.node).copied();
            let target = nodes.get(&edge.to.node).copied();
            match source {
                None => self.error(
                    &edge_path,
                    GraphValidationErrorKind::UnknownEndpoint {
                        node: edge.from.node.clone(),
                    },
                ),
                Some(node) if !node.accepts_output_port(&edge.from.port) => self.error(
                    &edge_path,
                    GraphValidationErrorKind::InvalidSourcePort {
                        node: edge.from.node.clone(),
                        kind: node.kind_name().into(),
                        port: edge.from.port.clone(),
                    },
                ),
                Some(_) => {}
            }
            if target.is_none() {
                self.error(
                    &edge_path,
                    GraphValidationErrorKind::UnknownEndpoint {
                        node: edge.to.node.clone(),
                    },
                );
            } else if !matches!(edge.to.port, EdgePort::Input) {
                self.error(
                    &edge_path,
                    GraphValidationErrorKind::InvalidTargetPort {
                        node: edge.to.node.clone(),
                        port: edge.to.port.clone(),
                    },
                );
            }

            if source.is_some() && target.is_some() {
                successors
                    .get_mut(&edge.from.node)
                    .expect("known source")
                    .insert(edge.to.node.clone());
                predecessors
                    .get_mut(&edge.to.node)
                    .expect("known target")
                    .insert(edge.from.node.clone());
                *incoming_count.get_mut(&edge.to.node).expect("known target") += 1;
            }
        }

        let mut entries = BTreeSet::new();
        for (index, entry) in graph.entries.iter().enumerate() {
            let entry_path = format!("{path}.entries[{index}]");
            if !entries.insert(entry.clone()) {
                self.error(
                    &entry_path,
                    GraphValidationErrorKind::DuplicateEntry {
                        node: entry.clone(),
                    },
                );
            }
            if !nodes.contains_key(entry) {
                self.error(
                    &entry_path,
                    GraphValidationErrorKind::UnknownEntry {
                        node: entry.clone(),
                    },
                );
            } else if predecessors
                .get(entry)
                .is_some_and(|edges| !edges.is_empty())
            {
                self.error(
                    &entry_path,
                    GraphValidationErrorKind::EntryHasIncoming {
                        node: entry.clone(),
                    },
                );
            }
        }

        let mut exit_names = BTreeSet::new();
        let mut exit_nodes = BTreeSet::new();
        for (index, exit) in graph.exits.iter().enumerate() {
            let exit_path = format!("{path}.exits[{index}]");
            if exit.name.trim().is_empty() {
                self.error(&exit_path, GraphValidationErrorKind::EmptyExitName);
            } else if !exit_names.insert(exit.name.clone()) {
                self.error(
                    &exit_path,
                    GraphValidationErrorKind::DuplicateExitName {
                        name: exit.name.clone(),
                    },
                );
            }
            match nodes.get(&exit.from.node).copied() {
                None => self.error(
                    &exit_path,
                    GraphValidationErrorKind::UnknownEndpoint {
                        node: exit.from.node.clone(),
                    },
                ),
                Some(node) if !node.accepts_output_port(&exit.from.port) => self.error(
                    &exit_path,
                    GraphValidationErrorKind::InvalidSourcePort {
                        node: exit.from.node.clone(),
                        kind: node.kind_name().into(),
                        port: exit.from.port.clone(),
                    },
                ),
                Some(_) => {
                    exit_nodes.insert(exit.from.node.clone());
                }
            }
        }

        let reachable = reachable_nodes(&entries, &successors);
        for node in &local_ids {
            if !reachable.contains(node) {
                self.error(
                    path,
                    GraphValidationErrorKind::UnreachableNode { node: node.clone() },
                );
            }
        }

        if let Some(cycle) = cyclic_nodes(&local_ids, &predecessors, &successors) {
            self.error(path, GraphValidationErrorKind::Cycle { nodes: cycle });
        }

        if !graph.exits.is_empty() {
            for node in &reachable {
                if successors.get(node).is_some_and(BTreeSet::is_empty)
                    && !exit_nodes.contains(node)
                {
                    self.error(
                        path,
                        GraphValidationErrorKind::UnlistedTerminal { node: node.clone() },
                    );
                }
            }
        }

        for node in &graph.nodes {
            if let GraphNode::Join(join) = node {
                let found = incoming_count.get(&join.id).copied().unwrap_or(0);
                if found < 2 {
                    self.error(
                        format!("{path}.node[{}]", join.id),
                        GraphValidationErrorKind::JoinNeedsMultipleInputs {
                            node: join.id.clone(),
                            found,
                        },
                    );
                }
            }
        }

        let dominators = compute_dominators(&reachable, &entries, &predecessors);
        for node in &graph.nodes {
            let node_id = node.id();
            let node_path = format!("{path}.node[{node_id}]");
            match node {
                GraphNode::Action(action) => {
                    if !action.step.bindings.is_empty() {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::InvalidAction {
                                node: node_id.into(),
                                message: "graph action embeds linear bindings; move them to action-node bindings".into(),
                            },
                        );
                    }
                    if let Err(message) = action.step.validate() {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::InvalidAction {
                                node: node_id.into(),
                                message,
                            },
                        );
                    }
                    if matches!(
                        action.step.action,
                        Action::ForEach { .. } | Action::ForEachGitCloneIfMissing { .. }
                    ) {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::LegacyControlAction {
                                node: node_id.into(),
                            },
                        );
                    }
                    let definition = definition_for_action(&action.step.action);
                    let mut normalized_targets = BTreeMap::<Vec<ContextPathSegment>, String>::new();
                    for (name, binding) in &action.bindings {
                        if name.trim().is_empty() {
                            self.error(
                                &node_path,
                                GraphValidationErrorKind::EmptyBindingName {
                                    node: node_id.into(),
                                },
                            );
                        }
                        if let Ok(segments) = parse_binding_target(name, BindingLimits::default()) {
                            if let Some(first) = normalized_targets.insert(segments, name.clone()) {
                                self.error(
                                    &node_path,
                                    GraphValidationErrorKind::DuplicateBindingTarget {
                                        node: node_id.into(),
                                        first,
                                        duplicate: name.clone(),
                                    },
                                );
                            }
                        }
                        let Some(expected) = resolve_input_field(&definition.input_schema, name)
                        else {
                            self.error(
                                &node_path,
                                GraphValidationErrorKind::UnknownBindingField {
                                    node: node_id.into(),
                                    field: name.clone(),
                                },
                            );
                            self.validate_binding(
                                binding,
                                node_id,
                                &node_path,
                                &local_ids,
                                inherited_visible,
                                active_loops,
                                &dominators,
                            );
                            continue;
                        };
                        self.validate_typed_action_binding(
                            name,
                            binding,
                            &expected,
                            node_id,
                            &node_path,
                            &local_ids,
                            inherited_visible,
                            active_loops,
                            &dominators,
                        );
                    }
                    for condition in [action.step.when.as_ref(), action.step.require.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        self.validate_step_condition(
                            condition,
                            node_id,
                            &node_path,
                            &local_ids,
                            inherited_visible,
                            active_loops,
                            active_aliases,
                            &dominators,
                        );
                    }
                }
                GraphNode::ForEach(control) => {
                    self.validate_binding(
                        &control.collection,
                        node_id,
                        &node_path,
                        &local_ids,
                        inherited_visible,
                        active_loops,
                        &dominators,
                    );
                    let collection_type = self.binding_static_type(
                        &control.collection,
                        node_id,
                        &node_path,
                        &local_ids,
                        inherited_visible,
                        active_loops,
                        &dominators,
                    );
                    let item_type = match collection_type.as_ref().map(|value| &value.value_type) {
                        Some(ContextType::Array { items }) => items.as_ref().clone(),
                        actual => {
                            self.error(
                                &node_path,
                                GraphValidationErrorKind::ForEachCollectionNotArray {
                                    node: node_id.into(),
                                    actual: actual.cloned(),
                                },
                            );
                            ContextType::Any
                        }
                    };
                    if control.concurrency == 0 {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::InvalidConcurrency {
                                node: node_id.into(),
                            },
                        );
                    }
                    self.validate_alias(&control.item_alias, node_id, &node_path, active_aliases);
                    if let Some(index_alias) = &control.index_alias {
                        self.validate_alias(index_alias, node_id, &node_path, active_aliases);
                        if index_alias == &control.item_alias {
                            self.error(
                                &node_path,
                                GraphValidationErrorKind::DuplicateAlias {
                                    node: node_id.into(),
                                    alias: index_alias.clone(),
                                },
                            );
                        }
                    }

                    let visible =
                        visible_for_child(node_id, inherited_visible, &local_ids, &dominators);
                    let mut loops = active_loops.clone();
                    let item_static_type = StaticBindingType {
                        value_type: item_type,
                        required: true,
                        nullable: false,
                        sensitivity: collection_type
                            .as_ref()
                            .map_or(Sensitivity::Public, |value| value.sensitivity),
                        allowed_values: Vec::new(),
                        positionally_optional: false,
                    };
                    loops.insert(node_id.into(), item_static_type);
                    let mut aliases = active_aliases.clone();
                    if !control.item_alias.trim().is_empty() {
                        aliases.insert(control.item_alias.clone(), FieldRef::loop_item(node_id));
                    }
                    if let Some(index_alias) = &control.index_alias {
                        if !index_alias.trim().is_empty() {
                            let index_scope = loop_index_scope(node_id);
                            loops.insert(
                                index_scope.clone(),
                                StaticBindingType {
                                    value_type: ContextType::Integer,
                                    required: true,
                                    nullable: false,
                                    sensitivity: Sensitivity::Public,
                                    allowed_values: Vec::new(),
                                    positionally_optional: false,
                                },
                            );
                            aliases.insert(index_alias.clone(), FieldRef::loop_item(index_scope));
                        }
                    }
                    self.validate_graph(
                        &control.body,
                        &format!("{node_path}.body"),
                        &visible,
                        &loops,
                        &aliases,
                    );
                }
                GraphNode::If(control) => {
                    self.validate_checked_expression(
                        &control.condition,
                        node_id,
                        &node_path,
                        &local_ids,
                        inherited_visible,
                        active_loops,
                        active_aliases,
                        &dominators,
                    );
                    let visible =
                        visible_for_child(node_id, inherited_visible, &local_ids, &dominators);
                    self.validate_graph(
                        &control.then_graph,
                        &format!("{node_path}.then"),
                        &visible,
                        active_loops,
                        active_aliases,
                    );
                    if let Some(graph) = &control.else_graph {
                        self.validate_graph(
                            graph,
                            &format!("{node_path}.else"),
                            &visible,
                            active_loops,
                            active_aliases,
                        );
                    }
                }
                GraphNode::Switch(control) => {
                    self.validate_binding(
                        &control.selector,
                        node_id,
                        &node_path,
                        &local_ids,
                        inherited_visible,
                        active_loops,
                        &dominators,
                    );
                    if let Binding::Template { template } = &control.selector {
                        if template.contains("{{") || template.contains("}}") {
                            self.error(
                                &node_path,
                                GraphValidationErrorKind::InvalidBindingValue {
                                    node: node_id.into(),
                                    field: "selector".into(),
                                    message: "legacy selector template contains placeholders"
                                        .into(),
                                },
                            );
                        }
                    }
                    let selector_type = self.binding_static_type(
                        &control.selector,
                        node_id,
                        &node_path,
                        &local_ids,
                        inherited_visible,
                        active_loops,
                        &dominators,
                    );
                    if selector_type
                        .as_ref()
                        .is_some_and(|selector| selector.sensitivity.is_secret())
                    {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::SecretBindingFlow {
                                node: node_id.into(),
                                field: "selector".into(),
                            },
                        );
                    }
                    if control.cases.is_empty() {
                        self.error(
                            &node_path,
                            GraphValidationErrorKind::EmptySwitchCases {
                                node: node_id.into(),
                            },
                        );
                    }
                    let mut case_ids = BTreeSet::new();
                    let mut values = BTreeSet::new();
                    let visible =
                        visible_for_child(node_id, inherited_visible, &local_ids, &dominators);
                    for (case_index, case) in control.cases.iter().enumerate() {
                        let case_path = format!("{node_path}.cases[{case_index}]");
                        if case.id.trim().is_empty() {
                            self.error(
                                &case_path,
                                GraphValidationErrorKind::EmptySwitchCaseId {
                                    node: node_id.into(),
                                },
                            );
                        } else if !case_ids.insert(case.id.clone()) {
                            self.error(
                                &case_path,
                                GraphValidationErrorKind::DuplicateSwitchCaseId {
                                    node: node_id.into(),
                                    case: case.id.clone(),
                                },
                            );
                        }
                        if case.values.is_empty() {
                            self.error(
                                &case_path,
                                GraphValidationErrorKind::EmptySwitchCaseValues {
                                    node: node_id.into(),
                                    case: case.id.clone(),
                                },
                            );
                        }
                        for value in &case.values {
                            if let Some(selector) = &selector_type {
                                let value_type = ContextType::infer(value);
                                if !switch_types_compatible(selector, &value_type) {
                                    self.error(
                                        &case_path,
                                        GraphValidationErrorKind::SwitchCaseTypeMismatch {
                                            node: node_id.into(),
                                            case: case.id.clone(),
                                            selector: selector.value_type.clone(),
                                            value: value_type,
                                        },
                                    );
                                }
                            }
                            let canonical = canonical_json(value);
                            if !values.insert(canonical.clone()) {
                                self.error(
                                    &case_path,
                                    GraphValidationErrorKind::DuplicateSwitchValue {
                                        node: node_id.into(),
                                        value: canonical,
                                    },
                                );
                            }
                        }
                        self.validate_graph(
                            &case.graph,
                            &format!("{case_path}.graph"),
                            &visible,
                            active_loops,
                            active_aliases,
                        );
                    }
                    if let Some(graph) = &control.default {
                        self.validate_graph(
                            graph,
                            &format!("{node_path}.default"),
                            &visible,
                            active_loops,
                            active_aliases,
                        );
                    }
                }
                GraphNode::Join(_) => {}
            }
        }
    }

    fn validate_alias(
        &mut self,
        alias: &str,
        node: &str,
        path: &str,
        active_aliases: &BTreeMap<String, FieldRef>,
    ) {
        if !is_identifier(alias) {
            self.error(
                path,
                GraphValidationErrorKind::InvalidAlias {
                    node: node.into(),
                    alias: alias.into(),
                },
            );
        } else if active_aliases.contains_key(alias) {
            self.error(
                path,
                GraphValidationErrorKind::ShadowedAlias {
                    node: node.into(),
                    alias: alias.into(),
                },
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_typed_action_binding(
        &mut self,
        input_name: &str,
        binding: &Binding,
        expected: &FieldSchema,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) {
        self.validate_binding(
            binding,
            consumer,
            path,
            local_ids,
            inherited_visible,
            active_loops,
            dominators,
        );

        let fully_value_checked = match binding {
            Binding::Literal { .. } | Binding::Template { .. } => true,
            Binding::Interpolated { parts } => parts
                .iter()
                .all(|part| matches!(part, TemplatePart::Literal { .. })),
            Binding::Field { .. } => false,
        };
        if fully_value_checked {
            let expected_owned = ResolvedSchemaOwned {
                value_type: expected.value_type.clone(),
                required: expected.required,
                nullable: expected.nullable,
                sensitivity: expected.sensitivity,
                allowed_values: expected.allowed_values.clone(),
            };
            if let Err(error) = resolve_binding(
                binding,
                &expected_owned,
                &ContextStore::default(),
                BindingLimits::default(),
            ) {
                self.error(
                    path,
                    GraphValidationErrorKind::InvalidBindingValue {
                        node: consumer.into(),
                        field: input_name.into(),
                        message: error.to_string(),
                    },
                );
                return;
            }
            // Literal-only bindings were checked against the complete runtime
            // contract, including nullable and semantic-format constraints.
            // Their inferred JSON string type intentionally carries no format
            // refinement, so a second structural check would be a false error.
            return;
        }

        let Some(actual) = self.binding_static_type(
            binding,
            consumer,
            path,
            local_ids,
            inherited_visible,
            active_loops,
            dominators,
        ) else {
            return;
        };
        let dynamic_interpolation = matches!(
            binding,
            Binding::Interpolated { parts }
                if parts.iter().any(|part| matches!(part, TemplatePart::Field { .. }))
        );
        let compatible = (matches!(actual.value_type, ContextType::Null) && expected.nullable)
            || expected.value_type.is_assignable_from(&actual.value_type)
            || (dynamic_interpolation
                && matches!(
                    expected.value_type,
                    ContextType::Any | ContextType::String { .. }
                ));
        if !compatible {
            self.error(
                path,
                GraphValidationErrorKind::BindingTypeMismatch {
                    node: consumer.into(),
                    field: input_name.into(),
                    expected: expected.value_type.clone(),
                    actual: actual.value_type,
                },
            );
        }
        if !enum_contract_accepts(&expected.allowed_values, &actual.allowed_values) {
            self.error(
                path,
                GraphValidationErrorKind::InvalidBindingValue {
                    node: consumer.into(),
                    field: input_name.into(),
                    message: if actual.allowed_values.is_empty() {
                        "source field is not constrained to the consumer's allowed values".into()
                    } else {
                        format!(
                            "source allowed values {} are not a subset of {}",
                            Value::Array(actual.allowed_values.clone()),
                            Value::Array(expected.allowed_values.clone())
                        )
                    },
                },
            );
        }
        if actual.sensitivity.is_secret() && expected.sensitivity != Sensitivity::Secret {
            self.error(
                path,
                GraphValidationErrorKind::SecretBindingFlow {
                    node: consumer.into(),
                    field: input_name.into(),
                },
            );
        }
        if expected.required && !actual.required && !actual.positionally_optional {
            self.error(
                path,
                GraphValidationErrorKind::BindingMayBeMissing {
                    node: consumer.into(),
                    field: input_name.into(),
                },
            );
        }
        if !expected.nullable && actual.nullable {
            self.error(
                path,
                GraphValidationErrorKind::BindingMayBeNull {
                    node: consumer.into(),
                    field: input_name.into(),
                },
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn binding_static_type(
        &mut self,
        binding: &Binding,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) -> Option<StaticBindingType> {
        match binding {
            Binding::Literal { value } => Some(StaticBindingType {
                value_type: ContextType::infer(value),
                required: true,
                nullable: value.is_null(),
                sensitivity: Sensitivity::Public,
                allowed_values: vec![value.clone()],
                positionally_optional: false,
            }),
            Binding::Template { .. } => Some(StaticBindingType {
                value_type: ContextType::STRING,
                required: true,
                nullable: false,
                sensitivity: Sensitivity::Public,
                allowed_values: Vec::new(),
                positionally_optional: false,
            }),
            Binding::Interpolated { parts } => {
                let sensitivity = parts.iter().fold(Sensitivity::Public, |current, part| {
                    let TemplatePart::Field { field } = part else {
                        return current;
                    };
                    self.field_static_type(
                        field,
                        consumer,
                        path,
                        local_ids,
                        inherited_visible,
                        active_loops,
                        dominators,
                    )
                    .map_or(current, |field| current.combine(field.sensitivity))
                });
                Some(StaticBindingType {
                    value_type: ContextType::STRING,
                    required: true,
                    nullable: false,
                    sensitivity,
                    allowed_values: Vec::new(),
                    positionally_optional: false,
                })
            }
            Binding::Field { field } => self.field_static_type(
                field,
                consumer,
                path,
                local_ids,
                inherited_visible,
                active_loops,
                dominators,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn field_static_type(
        &mut self,
        field: &FieldRef,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) -> Option<StaticBindingType> {
        if !field_ref_is_visible(
            field,
            consumer,
            local_ids,
            inherited_visible,
            active_loops,
            dominators,
        ) {
            return None;
        }
        match &field.scope {
            // The v2 task envelope does not yet declare a scenario input
            // schema, so scenario references are checked during task
            // integration when that envelope is available.
            ContextScope::Scenario => None,
            ContextScope::LoopItem { step_id } => {
                let root = active_loops.get(step_id)?;
                resolve_static_type(root, &field.segments).or_else(|| {
                    self.error(
                        path,
                        GraphValidationErrorKind::UnknownContextField {
                            consumer: consumer.into(),
                            producer: step_id.clone(),
                            field: display_segments(&field.segments),
                        },
                    );
                    None
                })
            }
            ContextScope::Step { step_id } => {
                let Some(schema) = self.global_output_schemas.get(step_id) else {
                    self.error(
                        path,
                        GraphValidationErrorKind::OutputSchemaUnavailable {
                            consumer: consumer.into(),
                            producer: step_id.clone(),
                        },
                    );
                    return None;
                };
                schema
                    .resolve_owned(&field.segments)
                    .map(|resolved| StaticBindingType {
                        value_type: resolved.value_type,
                        required: resolved.required,
                        nullable: resolved.nullable,
                        sensitivity: resolved.sensitivity,
                        allowed_values: resolved.allowed_values,
                        positionally_optional: source_is_optional_only_by_index(
                            schema,
                            &field.segments,
                        ),
                    })
                    .or_else(|| {
                        self.error(
                            path,
                            GraphValidationErrorKind::UnknownContextField {
                                consumer: consumer.into(),
                                producer: step_id.clone(),
                                field: display_segments(&field.segments),
                            },
                        );
                        None
                    })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_binding(
        &mut self,
        binding: &Binding,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) {
        match binding {
            Binding::Field { field } => self.validate_field_ref(
                field,
                consumer,
                path,
                local_ids,
                inherited_visible,
                active_loops,
                dominators,
            ),
            Binding::Interpolated { parts } => {
                for part in parts {
                    if let TemplatePart::Field { field } = part {
                        self.validate_field_ref(
                            field,
                            consumer,
                            path,
                            local_ids,
                            inherited_visible,
                            active_loops,
                            dominators,
                        );
                        if let Some(actual) = self.field_static_type(
                            field,
                            consumer,
                            path,
                            local_ids,
                            inherited_visible,
                            active_loops,
                            dominators,
                        ) {
                            if !matches!(
                                actual.value_type,
                                ContextType::String { .. }
                                    | ContextType::Boolean
                                    | ContextType::Integer
                                    | ContextType::Number
                            ) {
                                self.error(
                                    path,
                                    GraphValidationErrorKind::InterpolatedFieldNotScalar {
                                        consumer: consumer.into(),
                                        field: display_segments(&field.segments),
                                        actual: actual.value_type,
                                    },
                                );
                            }
                            if !actual.required && !actual.positionally_optional {
                                self.error(
                                    path,
                                    GraphValidationErrorKind::BindingMayBeMissing {
                                        node: consumer.into(),
                                        field: format!(
                                            "template:{}",
                                            display_segments(&field.segments)
                                        ),
                                    },
                                );
                            }
                            if actual.nullable {
                                self.error(
                                    path,
                                    GraphValidationErrorKind::BindingMayBeNull {
                                        node: consumer.into(),
                                        field: format!(
                                            "template:{}",
                                            display_segments(&field.segments)
                                        ),
                                    },
                                );
                            }
                        }
                    }
                }
            }
            Binding::Literal { .. } | Binding::Template { .. } => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_step_condition(
        &mut self,
        condition: &StepCondition,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        active_aliases: &BTreeMap<String, FieldRef>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut stack = vec![condition];
        while let Some(condition) = stack.pop() {
            match condition {
                StepCondition::ExitCode { step, codes } => {
                    self.validate_field_ref(
                        &FieldRef::step(step),
                        consumer,
                        path,
                        local_ids,
                        inherited_visible,
                        active_loops,
                        dominators,
                    );
                    match self.global_script_exit_codes.get(step).cloned() {
                        Some(Some(success_codes)) => {
                            for code in codes {
                                if !success_codes.contains(code) {
                                    self.error(
                                        path,
                                        GraphValidationErrorKind::ExitCodeNotSuccessful {
                                            consumer: consumer.into(),
                                            producer: step.clone(),
                                            code: *code,
                                        },
                                    );
                                }
                            }
                        }
                        Some(None) | None if self.global_ids.contains_key(step) => self.error(
                            path,
                            GraphValidationErrorKind::ExitCodeSourceNotRunScript {
                                consumer: consumer.into(),
                                producer: step.clone(),
                            },
                        ),
                        None | Some(None) => {}
                    }
                }
                StepCondition::Path { .. } => {}
                StepCondition::Expression { rule, .. } => self.validate_checked_expression(
                    rule,
                    consumer,
                    path,
                    local_ids,
                    inherited_visible,
                    active_loops,
                    active_aliases,
                    dominators,
                ),
                StepCondition::All { conditions } | StepCondition::Any { conditions } => {
                    stack.extend(conditions.iter());
                }
                StepCondition::Not { condition } => stack.push(condition),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_checked_expression(
        &mut self,
        expression: &ExpressionV1,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        active_aliases: &BTreeMap<String, FieldRef>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) {
        let limits = ExpressionLimits::default();
        if let Err(limit) = expression_stats(expression, limits) {
            self.resource_limit(path, limit.resource, limit.found, limit.limit);
            return;
        }
        self.validate_quantifier_aliases(expression, consumer, path, active_aliases);

        // Clone only after the iterative budget pass. The rewrite mirrors the
        // runtime alias semantics, but keeps quantifier locals lexical.
        let mut rewritten = expression.clone();
        rewrite_graph_aliases(
            &mut rewritten,
            active_aliases,
            &mut BTreeSet::new(),
            1,
            limits.max_depth,
        );
        let mut requested_scopes = BTreeSet::new();
        rewritten.visit_context_references(|field| {
            requested_scopes.insert(field.scope.clone());
        });
        let resolver = self.expression_context_store(
            consumer,
            local_ids,
            inherited_visible,
            active_loops,
            dominators,
            &requested_scopes,
        );
        if let Err(diagnostics) = check_rule(rewritten, &resolver, limits) {
            for diagnostic in diagnostics {
                self.error(
                    path,
                    GraphValidationErrorKind::InvalidExpression {
                        node: consumer.into(),
                        code: format!("{:?}", diagnostic.code),
                        location: diagnostic.location,
                        message: diagnostic.message,
                    },
                );
            }
        }
    }

    fn validate_quantifier_aliases(
        &mut self,
        root: &ExpressionV1,
        consumer: &str,
        path: &str,
        active_aliases: &BTreeMap<String, FieldRef>,
    ) {
        let mut stack = vec![root];
        while let Some(expression) = stack.pop() {
            match expression {
                ExpressionV1::Literal { .. }
                | ExpressionV1::Ref { .. }
                | ExpressionV1::Exists { .. } => {}
                ExpressionV1::All { expressions } | ExpressionV1::Any { expressions } => {
                    stack.extend(expressions.iter());
                }
                ExpressionV1::Not { expression }
                | ExpressionV1::IsNull { expression }
                | ExpressionV1::IsEmpty { expression }
                | ExpressionV1::Matches {
                    value: expression, ..
                } => stack.push(expression),
                ExpressionV1::Compare { left, right, .. } => {
                    stack.push(left);
                    stack.push(right);
                }
                ExpressionV1::Contains { value, needle } => {
                    stack.push(value);
                    stack.push(needle);
                }
                ExpressionV1::StartsWith { value, prefix } => {
                    stack.push(value);
                    stack.push(prefix);
                }
                ExpressionV1::EndsWith { value, suffix } => {
                    stack.push(value);
                    stack.push(suffix);
                }
                ExpressionV1::In { needle, collection } => {
                    stack.push(needle);
                    stack.push(collection);
                }
                ExpressionV1::Quantifier {
                    collection,
                    binding,
                    predicate,
                    ..
                } => {
                    if !is_identifier(binding) {
                        self.error(
                            path,
                            GraphValidationErrorKind::InvalidAlias {
                                node: consumer.into(),
                                alias: binding.clone(),
                            },
                        );
                    } else if active_aliases.contains_key(binding) {
                        self.error(
                            path,
                            GraphValidationErrorKind::ShadowedAlias {
                                node: consumer.into(),
                                alias: binding.clone(),
                            },
                        );
                    }
                    stack.push(collection);
                    stack.push(predicate);
                }
            }
        }
    }

    fn expression_context_store(
        &self,
        consumer: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
        requested_scopes: &BTreeSet<ContextScope>,
    ) -> ContextStore {
        let mut visible = inherited_visible.clone();
        if let Some(local_dominators) = dominators.get(consumer) {
            visible.extend(
                local_dominators
                    .iter()
                    .filter(|step| step.as_str() != consumer && local_ids.contains(*step))
                    .cloned(),
            );
        }

        let mut store = ContextStore::default();
        for step_id in visible {
            let scope = ContextScope::Step {
                step_id: step_id.clone(),
            };
            if !requested_scopes.contains(&scope) {
                continue;
            }
            let Some(schema) = self.global_output_schemas.get(&step_id) else {
                continue;
            };
            store.insert(
                scope,
                ContextValue::new(Value::Null, ContextProvenance::step(&step_id))
                    .with_schema(schema.clone()),
            );
        }
        for (loop_id, value_type) in active_loops {
            let scope = ContextScope::LoopItem {
                step_id: loop_id.clone(),
            };
            if !requested_scopes.contains(&scope) {
                continue;
            }
            store.insert(
                scope,
                ContextValue::new(
                    Value::Null,
                    ContextProvenance {
                        origin: ContextOrigin::LoopItem {
                            step_id: loop_id.clone(),
                            index: 0,
                        },
                        inputs: Vec::new(),
                        operation: Some("graph-static-schema".into()),
                    },
                )
                .with_type(value_type.value_type.clone())
                .sensitive(value_type.sensitivity),
            );
        }
        store
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_field_ref(
        &mut self,
        field: &FieldRef,
        consumer: &str,
        path: &str,
        local_ids: &BTreeSet<String>,
        inherited_visible: &BTreeSet<String>,
        active_loops: &BTreeMap<String, StaticBindingType>,
        dominators: &BTreeMap<String, BTreeSet<String>>,
    ) {
        match &field.scope {
            ContextScope::Scenario => self.error(
                path,
                GraphValidationErrorKind::UnknownContextField {
                    consumer: consumer.into(),
                    producer: "scenario input".into(),
                    field: display_segments(&field.segments),
                },
            ),
            ContextScope::LoopItem { step_id } => {
                if !active_loops.contains_key(step_id) {
                    self.error(
                        path,
                        GraphValidationErrorKind::LoopContextNotVisible {
                            consumer: consumer.into(),
                            loop_node: step_id.clone(),
                        },
                    );
                }
            }
            ContextScope::Step { step_id } => {
                let locally_visible = local_ids.contains(step_id)
                    && step_id != consumer
                    && dominators
                        .get(consumer)
                        .is_some_and(|set| set.contains(step_id));
                if inherited_visible.contains(step_id) || locally_visible {
                    return;
                }
                let kind = if self.global_ids.contains_key(step_id) {
                    GraphValidationErrorKind::ContextNotVisible {
                        consumer: consumer.into(),
                        producer: step_id.clone(),
                    }
                } else {
                    GraphValidationErrorKind::UnknownContextStep {
                        consumer: consumer.into(),
                        producer: step_id.clone(),
                    }
                };
                self.error(path, kind);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct StaticBindingType {
    value_type: ContextType,
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
    allowed_values: Vec<Value>,
    /// The structural type is optional only because a concrete array index may
    /// be out of bounds. Runtime resolution remains fail-closed before the
    /// consumer action; named optional fields never set this flag.
    positionally_optional: bool,
}

#[derive(Default)]
struct GraphPreflightBudget {
    graphs: usize,
    nodes: usize,
    edges: usize,
    endpoints: usize,
    bindings: usize,
    switch_cases: usize,
    switch_values: usize,
    expression_nodes: usize,
    value_nodes: usize,
    template_parts: usize,
    condition_nodes: usize,
    exit_codes: usize,
    payload_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct StaticLimitExceeded {
    resource: &'static str,
    found: usize,
    limit: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct StaticTreeStats {
    nodes: usize,
    payload_bytes: usize,
}

struct LimitedByteCounter {
    written: usize,
    attempted: usize,
    limit: usize,
}

impl Write for LimitedByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let attempted = self.written.saturating_add(bytes.len());
        self.attempted = attempted;
        if attempted > self.limit {
            return Err(io::Error::other("serialized component byte limit exceeded"));
        }
        self.written = attempted;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_payload_bytes<T: Serialize>(value: &T) -> Result<usize, StaticLimitExceeded> {
    let mut counter = LimitedByteCounter {
        written: 0,
        attempted: 0,
        limit: GRAPH_MAX_SERIALIZED_COMPONENT_BYTES,
    };
    serde_json::to_writer(&mut counter, value).map_err(|_| StaticLimitExceeded {
        resource: "serialized component bytes",
        found: counter.attempted,
        limit: GRAPH_MAX_SERIALIZED_COMPONENT_BYTES,
    })?;
    Ok(counter.written)
}

enum ExpressionWalkItem<'a> {
    Expression(&'a ExpressionV1, usize),
    Value(&'a ExpressionValue, usize),
}

fn expression_stats(
    root: &ExpressionV1,
    limits: ExpressionLimits,
) -> Result<StaticTreeStats, StaticLimitExceeded> {
    let binding_limits = BindingLimits::default();
    let mut nodes = 0usize;
    let mut payload_bytes = 0usize;
    let mut stack = vec![ExpressionWalkItem::Expression(root, 1)];
    while let Some(item) = stack.pop() {
        let depth = match item {
            ExpressionWalkItem::Expression(_, depth) | ExpressionWalkItem::Value(_, depth) => depth,
        };
        if depth > limits.max_depth {
            return Err(StaticLimitExceeded {
                resource: "expression depth",
                found: depth,
                limit: limits.max_depth,
            });
        }
        nodes = nodes.saturating_add(1);
        if nodes > limits.max_nodes {
            return Err(StaticLimitExceeded {
                resource: "expression nodes",
                found: nodes,
                limit: limits.max_nodes,
            });
        }

        match item {
            ExpressionWalkItem::Expression(expression, depth) => match expression {
                ExpressionV1::Literal { value } => match value {
                    ExpressionValue::List(values) => {
                        ensure_pending_limit(
                            nodes,
                            stack.len(),
                            values.len(),
                            limits.max_nodes,
                            "expression nodes",
                        )?;
                        stack.extend(
                            values
                                .iter()
                                .map(|value| ExpressionWalkItem::Value(value, depth + 1)),
                        );
                    }
                    ExpressionValue::Object(values) => {
                        ensure_pending_limit(
                            nodes,
                            stack.len(),
                            values.len(),
                            limits.max_nodes,
                            "expression nodes",
                        )?;
                        for (name, value) in values {
                            if name.len() > limits.max_string_bytes {
                                return Err(StaticLimitExceeded {
                                    resource: "expression object key bytes",
                                    found: name.len(),
                                    limit: limits.max_string_bytes,
                                });
                            }
                            payload_bytes = payload_bytes.saturating_add(name.len());
                            stack.push(ExpressionWalkItem::Value(value, depth + 1));
                        }
                    }
                    ExpressionValue::String(value) => {
                        if value.len() > limits.max_string_bytes {
                            return Err(StaticLimitExceeded {
                                resource: "expression string bytes",
                                found: value.len(),
                                limit: limits.max_string_bytes,
                            });
                        }
                        payload_bytes = payload_bytes.saturating_add(value.len());
                    }
                    _ => {}
                },
                ExpressionV1::Ref { reference } | ExpressionV1::Exists { reference } => {
                    payload_bytes = payload_bytes.saturating_add(validate_reference_budget(
                        reference,
                        limits,
                        binding_limits,
                    )?);
                }
                ExpressionV1::All { expressions } | ExpressionV1::Any { expressions } => {
                    ensure_pending_limit(
                        nodes,
                        stack.len(),
                        expressions.len(),
                        limits.max_nodes,
                        "expression nodes",
                    )?;
                    stack.extend(
                        expressions
                            .iter()
                            .map(|child| ExpressionWalkItem::Expression(child, depth + 1)),
                    );
                }
                ExpressionV1::Not { expression }
                | ExpressionV1::IsNull { expression }
                | ExpressionV1::IsEmpty { expression } => {
                    stack.push(ExpressionWalkItem::Expression(expression, depth + 1));
                }
                ExpressionV1::Compare { left, right, .. } => {
                    stack.push(ExpressionWalkItem::Expression(left, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(right, depth + 1));
                }
                ExpressionV1::Contains { value, needle } => {
                    stack.push(ExpressionWalkItem::Expression(value, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(needle, depth + 1));
                }
                ExpressionV1::StartsWith { value, prefix } => {
                    stack.push(ExpressionWalkItem::Expression(value, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(prefix, depth + 1));
                }
                ExpressionV1::EndsWith { value, suffix } => {
                    stack.push(ExpressionWalkItem::Expression(value, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(suffix, depth + 1));
                }
                ExpressionV1::Matches { value, pattern } => {
                    if pattern.len() > limits.max_regex_pattern_bytes {
                        return Err(StaticLimitExceeded {
                            resource: "regex pattern bytes",
                            found: pattern.len(),
                            limit: limits.max_regex_pattern_bytes,
                        });
                    }
                    payload_bytes = payload_bytes.saturating_add(pattern.len());
                    stack.push(ExpressionWalkItem::Expression(value, depth + 1));
                }
                ExpressionV1::In { needle, collection } => {
                    stack.push(ExpressionWalkItem::Expression(needle, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(collection, depth + 1));
                }
                ExpressionV1::Quantifier {
                    collection,
                    binding,
                    predicate,
                    ..
                } => {
                    if binding.len() > limits.max_string_bytes {
                        return Err(StaticLimitExceeded {
                            resource: "quantifier binding bytes",
                            found: binding.len(),
                            limit: limits.max_string_bytes,
                        });
                    }
                    payload_bytes = payload_bytes.saturating_add(binding.len());
                    stack.push(ExpressionWalkItem::Expression(collection, depth + 1));
                    stack.push(ExpressionWalkItem::Expression(predicate, depth + 1));
                }
            },
            ExpressionWalkItem::Value(value, depth) => match value {
                ExpressionValue::String(value) => {
                    if value.len() > limits.max_string_bytes {
                        return Err(StaticLimitExceeded {
                            resource: "expression string bytes",
                            found: value.len(),
                            limit: limits.max_string_bytes,
                        });
                    }
                    payload_bytes = payload_bytes.saturating_add(value.len());
                }
                ExpressionValue::List(values) => {
                    ensure_pending_limit(
                        nodes,
                        stack.len(),
                        values.len(),
                        limits.max_nodes,
                        "expression nodes",
                    )?;
                    stack.extend(
                        values
                            .iter()
                            .map(|value| ExpressionWalkItem::Value(value, depth + 1)),
                    );
                }
                ExpressionValue::Object(values) => {
                    ensure_pending_limit(
                        nodes,
                        stack.len(),
                        values.len(),
                        limits.max_nodes,
                        "expression nodes",
                    )?;
                    for (name, value) in values {
                        if name.len() > limits.max_string_bytes {
                            return Err(StaticLimitExceeded {
                                resource: "expression object key bytes",
                                found: name.len(),
                                limit: limits.max_string_bytes,
                            });
                        }
                        payload_bytes = payload_bytes.saturating_add(name.len());
                        stack.push(ExpressionWalkItem::Value(value, depth + 1));
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StaticTreeStats {
        nodes,
        payload_bytes,
    })
}

fn ensure_pending_limit(
    visited: usize,
    pending: usize,
    adding: usize,
    limit: usize,
    resource: &'static str,
) -> Result<(), StaticLimitExceeded> {
    let found = visited.saturating_add(pending).saturating_add(adding);
    if found > limit {
        Err(StaticLimitExceeded {
            resource,
            found,
            limit,
        })
    } else {
        Ok(())
    }
}

fn validate_reference_budget(
    reference: &ReferenceV1,
    limits: ExpressionLimits,
    binding_limits: BindingLimits,
) -> Result<usize, StaticLimitExceeded> {
    let mut payload_bytes = 0usize;
    match reference {
        ReferenceV1::Context { field } => {
            let scope_id = match &field.scope {
                ContextScope::Scenario => None,
                ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                    Some(step_id)
                }
            };
            if let Some(scope_id) = scope_id {
                if scope_id.len() > limits.max_string_bytes {
                    return Err(StaticLimitExceeded {
                        resource: "expression context scope bytes",
                        found: scope_id.len(),
                        limit: limits.max_string_bytes,
                    });
                }
                payload_bytes = payload_bytes.saturating_add(scope_id.len());
            }
            if field.segments.len() > binding_limits.max_path_segments {
                return Err(StaticLimitExceeded {
                    resource: "expression context path segments",
                    found: field.segments.len(),
                    limit: binding_limits.max_path_segments,
                });
            }
            for segment in &field.segments {
                if let ContextPathSegment::Field { name } = segment {
                    if name.len() > limits.max_string_bytes {
                        return Err(StaticLimitExceeded {
                            resource: "expression context path bytes",
                            found: name.len(),
                            limit: limits.max_string_bytes,
                        });
                    }
                    payload_bytes = payload_bytes.saturating_add(name.len());
                }
            }
        }
        ReferenceV1::Local { binding, path } => {
            if binding.len() > limits.max_string_bytes {
                return Err(StaticLimitExceeded {
                    resource: "expression local binding bytes",
                    found: binding.len(),
                    limit: limits.max_string_bytes,
                });
            }
            payload_bytes = payload_bytes.saturating_add(binding.len());
            if path.len() > binding_limits.max_path_segments {
                return Err(StaticLimitExceeded {
                    resource: "expression local path segments",
                    found: path.len(),
                    limit: binding_limits.max_path_segments,
                });
            }
            for segment in path {
                if segment.len() > limits.max_string_bytes {
                    return Err(StaticLimitExceeded {
                        resource: "expression local path bytes",
                        found: segment.len(),
                        limit: limits.max_string_bytes,
                    });
                }
                payload_bytes = payload_bytes.saturating_add(segment.len());
            }
        }
    }
    Ok(payload_bytes)
}

fn field_ref_payload_bytes(field: &FieldRef) -> Result<usize, StaticLimitExceeded> {
    let mut payload_bytes = match &field.scope {
        ContextScope::Scenario => 0,
        ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
            if step_id.len() > GRAPH_MAX_LITERAL_STRING_BYTES {
                return Err(StaticLimitExceeded {
                    resource: "binding context scope bytes",
                    found: step_id.len(),
                    limit: GRAPH_MAX_LITERAL_STRING_BYTES,
                });
            }
            step_id.len()
        }
    };
    for segment in &field.segments {
        if let ContextPathSegment::Field { name } = segment {
            if name.len() > GRAPH_MAX_LITERAL_STRING_BYTES {
                return Err(StaticLimitExceeded {
                    resource: "binding context path bytes",
                    found: name.len(),
                    limit: GRAPH_MAX_LITERAL_STRING_BYTES,
                });
            }
            payload_bytes = payload_bytes.saturating_add(name.len());
        }
    }
    Ok(payload_bytes)
}

fn json_value_stats(root: &Value) -> Result<StaticTreeStats, StaticLimitExceeded> {
    let mut nodes = 0usize;
    let mut payload_bytes = 0usize;
    let mut stack = vec![(root, 1usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > GRAPH_MAX_VALUE_DEPTH {
            return Err(StaticLimitExceeded {
                resource: "literal value depth",
                found: depth,
                limit: GRAPH_MAX_VALUE_DEPTH,
            });
        }
        nodes = nodes.saturating_add(1);
        if nodes > GRAPH_MAX_VALUE_NODES {
            return Err(StaticLimitExceeded {
                resource: "literal value nodes",
                found: nodes,
                limit: GRAPH_MAX_VALUE_NODES,
            });
        }
        match value {
            Value::String(value) => {
                if value.len() > GRAPH_MAX_LITERAL_STRING_BYTES {
                    return Err(StaticLimitExceeded {
                        resource: "literal string bytes",
                        found: value.len(),
                        limit: GRAPH_MAX_LITERAL_STRING_BYTES,
                    });
                }
                payload_bytes = payload_bytes.saturating_add(value.len());
            }
            Value::Array(values) => {
                ensure_pending_limit(
                    nodes,
                    stack.len(),
                    values.len(),
                    GRAPH_MAX_VALUE_NODES,
                    "literal value nodes",
                )?;
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                ensure_pending_limit(
                    nodes,
                    stack.len(),
                    values.len(),
                    GRAPH_MAX_VALUE_NODES,
                    "literal value nodes",
                )?;
                for (name, value) in values {
                    if name.len() > GRAPH_MAX_LITERAL_STRING_BYTES {
                        return Err(StaticLimitExceeded {
                            resource: "literal object key bytes",
                            found: name.len(),
                            limit: GRAPH_MAX_LITERAL_STRING_BYTES,
                        });
                    }
                    payload_bytes = payload_bytes.saturating_add(name.len());
                    stack.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(StaticTreeStats {
        nodes,
        payload_bytes,
    })
}

fn rewrite_graph_aliases(
    expression: &mut ExpressionV1,
    aliases: &BTreeMap<String, FieldRef>,
    quantifier_bindings: &mut BTreeSet<String>,
    depth: usize,
    max_depth: usize,
) {
    if depth > max_depth {
        return;
    }
    match expression {
        ExpressionV1::Literal { .. } => {}
        ExpressionV1::Ref { reference } | ExpressionV1::Exists { reference } => {
            let ReferenceV1::Local { binding, path } = reference else {
                return;
            };
            if quantifier_bindings.contains(binding) {
                return;
            }
            let Some(mut field) = aliases.get(binding).cloned() else {
                return;
            };
            field
                .segments
                .extend(path.iter().cloned().map(ContextPathSegment::field));
            *reference = ReferenceV1::Context { field };
        }
        ExpressionV1::All { expressions } | ExpressionV1::Any { expressions } => {
            for expression in expressions {
                rewrite_graph_aliases(
                    expression,
                    aliases,
                    quantifier_bindings,
                    depth + 1,
                    max_depth,
                );
            }
        }
        ExpressionV1::Not { expression }
        | ExpressionV1::IsNull { expression }
        | ExpressionV1::IsEmpty { expression } => rewrite_graph_aliases(
            expression,
            aliases,
            quantifier_bindings,
            depth + 1,
            max_depth,
        ),
        ExpressionV1::Compare { left, right, .. } => {
            rewrite_graph_aliases(left, aliases, quantifier_bindings, depth + 1, max_depth);
            rewrite_graph_aliases(right, aliases, quantifier_bindings, depth + 1, max_depth);
        }
        ExpressionV1::Contains { value, needle } => {
            rewrite_graph_aliases(value, aliases, quantifier_bindings, depth + 1, max_depth);
            rewrite_graph_aliases(needle, aliases, quantifier_bindings, depth + 1, max_depth);
        }
        ExpressionV1::StartsWith { value, prefix } => {
            rewrite_graph_aliases(value, aliases, quantifier_bindings, depth + 1, max_depth);
            rewrite_graph_aliases(prefix, aliases, quantifier_bindings, depth + 1, max_depth);
        }
        ExpressionV1::EndsWith { value, suffix } => {
            rewrite_graph_aliases(value, aliases, quantifier_bindings, depth + 1, max_depth);
            rewrite_graph_aliases(suffix, aliases, quantifier_bindings, depth + 1, max_depth);
        }
        ExpressionV1::Matches { value, .. } => {
            rewrite_graph_aliases(value, aliases, quantifier_bindings, depth + 1, max_depth);
        }
        ExpressionV1::In { needle, collection } => {
            rewrite_graph_aliases(needle, aliases, quantifier_bindings, depth + 1, max_depth);
            rewrite_graph_aliases(
                collection,
                aliases,
                quantifier_bindings,
                depth + 1,
                max_depth,
            );
        }
        ExpressionV1::Quantifier {
            collection,
            binding,
            predicate,
            ..
        } => {
            rewrite_graph_aliases(
                collection,
                aliases,
                quantifier_bindings,
                depth + 1,
                max_depth,
            );
            let was_present = !quantifier_bindings.insert(binding.clone());
            rewrite_graph_aliases(
                predicate,
                aliases,
                quantifier_bindings,
                depth + 1,
                max_depth,
            );
            if !was_present {
                quantifier_bindings.remove(binding);
            }
        }
    }
}

fn loop_index_scope(loop_id: &str) -> String {
    format!("{loop_id}::index")
}

fn field_ref_is_visible(
    field: &FieldRef,
    consumer: &str,
    local_ids: &BTreeSet<String>,
    inherited_visible: &BTreeSet<String>,
    active_loops: &BTreeMap<String, StaticBindingType>,
    dominators: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    match &field.scope {
        // WorkflowGraph v3 does not yet declare a scenario-input schema.
        // Reject these references during validation instead of deferring an
        // untyped lookup to runtime.
        ContextScope::Scenario => false,
        ContextScope::LoopItem { step_id } => active_loops.contains_key(step_id),
        ContextScope::Step { step_id } => {
            inherited_visible.contains(step_id)
                || (local_ids.contains(step_id)
                    && step_id != consumer
                    && dominators
                        .get(consumer)
                        .is_some_and(|set| set.contains(step_id)))
        }
    }
}

fn resolve_static_type(
    root: &StaticBindingType,
    segments: &[ContextPathSegment],
) -> Option<StaticBindingType> {
    let mut value_type = root.value_type.clone();
    let mut required = root.required;
    let mut nullable = root.nullable;
    let mut sensitivity = root.sensitivity;
    let mut allowed_values = root.allowed_values.clone();
    let mut positionally_optional = root.positionally_optional;
    for segment in segments {
        match (segment, value_type) {
            (ContextPathSegment::Field { name }, ContextType::Object { schema }) => {
                let field = schema.fields.get(name)?;
                if !field.required {
                    positionally_optional = false;
                }
                required &= field.required;
                nullable |= field.nullable;
                sensitivity = sensitivity.combine(field.sensitivity);
                allowed_values = field.allowed_values.clone();
                value_type = field.value_type.clone();
            }
            (ContextPathSegment::Index { .. }, ContextType::Array { items }) => {
                // A concrete array position is never guaranteed by a structural
                // schema, even when the array field itself is required.
                if required {
                    positionally_optional = true;
                }
                required = false;
                allowed_values.clear();
                value_type = *items;
            }
            _ => return None,
        }
    }
    let sensitivity = sensitivity.combine(context_type_sensitivity(&value_type));
    Some(StaticBindingType {
        value_type,
        required,
        nullable,
        sensitivity,
        allowed_values,
        positionally_optional,
    })
}

fn enum_contract_accepts(expected: &[Value], actual: &[Value]) -> bool {
    expected.is_empty()
        || (!actual.is_empty()
            && actual
                .iter()
                .all(|value| expected.iter().any(|allowed| allowed == value)))
}

fn source_is_optional_only_by_index(
    schema: &ObjectSchema,
    segments: &[ContextPathSegment],
) -> bool {
    let mut value_type = ContextType::object(schema.clone());
    let mut saw_index = false;
    for segment in segments {
        match (segment, value_type) {
            (ContextPathSegment::Field { name }, ContextType::Object { schema }) => {
                let field = match schema.fields.get(name) {
                    Some(field) => field,
                    None => return false,
                };
                if !field.required || field.nullable {
                    return false;
                }
                value_type = field.value_type.clone();
            }
            (ContextPathSegment::Index { .. }, ContextType::Array { items }) => {
                saw_index = true;
                value_type = *items;
            }
            _ => return false,
        }
    }
    saw_index
}

fn context_type_sensitivity(value_type: &ContextType) -> Sensitivity {
    match value_type {
        ContextType::Array { items } => context_type_sensitivity(items),
        ContextType::Object { schema } => {
            schema
                .fields
                .values()
                .fold(Sensitivity::Public, |current, field| {
                    current
                        .combine(field.sensitivity)
                        .combine(context_type_sensitivity(&field.value_type))
                })
        }
        ContextType::Any
        | ContextType::Null
        | ContextType::Boolean
        | ContextType::Integer
        | ContextType::Number
        | ContextType::String { .. } => Sensitivity::Public,
    }
}

fn resolve_input_field(schema: &ObjectSchema, target: &str) -> Option<FieldSchema> {
    let segments = parse_binding_target(target, BindingLimits::default()).ok()?;
    if segments
        .iter()
        .any(|segment| matches!(segment, ContextPathSegment::Index { .. }))
    {
        return None;
    }
    let resolved = schema.resolve_input_target_owned(&segments)?;
    Some(FieldSchema {
        value_type: resolved.value_type,
        required: resolved.required,
        nullable: resolved.nullable,
        description: None,
        sensitivity: resolved.sensitivity,
        allowed_values: resolved.allowed_values,
    })
}

fn display_segments(segments: &[ContextPathSegment]) -> String {
    let mut output = String::new();
    for segment in segments {
        match segment {
            ContextPathSegment::Field { name } => {
                if !output.is_empty() {
                    output.push('.');
                }
                output.push_str(name);
            }
            ContextPathSegment::Index { index } => {
                output.push('[');
                output.push_str(&index.to_string());
                output.push(']');
            }
        }
    }
    if output.is_empty() {
        "<root>".into()
    } else {
        output
    }
}

fn visible_for_child(
    control: &str,
    inherited: &BTreeSet<String>,
    local_ids: &BTreeSet<String>,
    dominators: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut visible = inherited.clone();
    if let Some(dominators) = dominators.get(control) {
        visible.extend(
            dominators
                .iter()
                .filter(|producer| producer.as_str() != control && local_ids.contains(*producer))
                .cloned(),
        );
    }
    visible
}

fn reachable_nodes(
    entries: &BTreeSet<String>,
    successors: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut queue = entries
        .iter()
        .filter(|entry| successors.contains_key(*entry))
        .cloned()
        .collect::<VecDeque<_>>();
    while let Some(node) = queue.pop_front() {
        if !reachable.insert(node.clone()) {
            continue;
        }
        if let Some(next) = successors.get(&node) {
            queue.extend(next.iter().cloned());
        }
    }
    reachable
}

fn cyclic_nodes(
    nodes: &BTreeSet<String>,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    successors: &BTreeMap<String, BTreeSet<String>>,
) -> Option<Vec<String>> {
    let mut indegree = nodes
        .iter()
        .map(|node| {
            (
                node.clone(),
                predecessors.get(node).map_or(0, BTreeSet::len),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut queue = indegree
        .iter()
        .filter_map(|(node, degree)| (*degree == 0).then_some(node.clone()))
        .collect::<VecDeque<_>>();
    let mut visited = BTreeSet::new();
    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.clone()) {
            continue;
        }
        if let Some(next) = successors.get(&node) {
            for target in next {
                let degree = indegree.get_mut(target).expect("known graph node");
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    queue.push_back(target.clone());
                }
            }
        }
    }
    if visited.len() == nodes.len() {
        None
    } else {
        Some(nodes.difference(&visited).cloned().collect())
    }
}

fn compute_dominators(
    reachable: &BTreeSet<String>,
    entries: &BTreeSet<String>,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut dominators = reachable
        .iter()
        .map(|node| {
            let initial = if entries.contains(node) {
                BTreeSet::from([node.clone()])
            } else {
                reachable.clone()
            };
            (node.clone(), initial)
        })
        .collect::<BTreeMap<_, _>>();

    loop {
        let mut changed = false;
        for node in reachable {
            if entries.contains(node) {
                continue;
            }
            let incoming = predecessors
                .get(node)
                .into_iter()
                .flatten()
                .filter(|predecessor| reachable.contains(*predecessor))
                .collect::<Vec<_>>();
            let mut next = incoming.first().map_or_else(BTreeSet::new, |first| {
                dominators.get(*first).cloned().unwrap_or_default()
            });
            for predecessor in incoming.iter().skip(1) {
                let predecessor_dominators =
                    dominators.get(*predecessor).cloned().unwrap_or_default();
                next = next
                    .intersection(&predecessor_dominators)
                    .cloned()
                    .collect();
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
    dominators
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into())
}

fn switch_types_compatible(selector: &StaticBindingType, value: &ContextType) -> bool {
    if matches!(value, ContextType::Null) {
        return selector.nullable || matches!(selector.value_type, ContextType::Any);
    }
    selector.value_type.is_assignable_from(value) || value.is_assignable_from(&selector.value_type)
}

/// Unicode-aware identifier check used for expression and loop aliases.
fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::block::{default_step, ActionKind};
    use crate::automation::context::FieldRef;
    use crate::automation::expression::{CollectionQuantifier, ExpressionValue, ReferenceV1};
    use crate::automation::task::{
        AuthPolicy, ElevationPolicy, RuleOutcomePolicy, ScriptInterpreter,
    };

    fn step(id: &str) -> Step {
        Step {
            id: id.into(),
            name: format!("Step {id}"),
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

    fn action(id: &str) -> GraphNode {
        GraphNode::Action(Box::new(ActionNode {
            step: step(id),
            bindings: BTreeMap::new(),
        }))
    }

    fn action_with_ref(id: &str, field: FieldRef) -> GraphNode {
        let mut consumer = step(id);
        consumer.action = Action::GitClone {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: None,
        };
        GraphNode::Action(Box::new(ActionNode {
            step: consumer,
            bindings: BTreeMap::from([("repo".into(), Binding::field(field))]),
        }))
    }

    fn identifier_action_with_ref(id: &str, field: FieldRef) -> GraphNode {
        let mut consumer = step(id);
        consumer.action = Action::BrewInstall {
            package: "placeholder".into(),
            cask: false,
        };
        GraphNode::Action(Box::new(ActionNode {
            step: consumer,
            bindings: BTreeMap::from([("package".into(), Binding::field(field))]),
        }))
    }

    fn graph(entries: &[&str], nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> WorkflowGraph {
        WorkflowGraph {
            entries: entries.iter().map(|entry| (*entry).into()).collect(),
            nodes,
            edges,
            ..WorkflowGraph::default()
        }
    }

    fn has_error(
        result: &Result<(), Vec<GraphValidationError>>,
        predicate: impl Fn(&GraphValidationErrorKind) -> bool,
    ) -> bool {
        result
            .as_ref()
            .err()
            .is_some_and(|errors| errors.iter().any(|error| predicate(&error.kind)))
    }

    fn validate_interpreter_source(field: FieldSchema) -> Result<(), Vec<GraphValidationError>> {
        let source = action("source");
        let script = default_step(ActionKind::RunScript, "script").unwrap();
        let root = graph(
            &["source"],
            vec![
                source,
                GraphNode::Action(Box::new(ActionNode {
                    step: script,
                    bindings: BTreeMap::from([(
                        "interpreter".into(),
                        Binding::field(FieldRef::step("source").field("value")),
                    )]),
                })),
            ],
            vec![GraphEdge::new("source", EdgePort::Success, "script")],
        );

        let mut validator = GraphValidator::default();
        if validator.preflight(&root) {
            validator.collect_ids(&root, "graph");
            validator.global_output_schemas.insert(
                "source".into(),
                ObjectSchema::new("test.interpreter-source@1").with_field("value", field),
            );
            validator.validate_graph(
                &root,
                "graph",
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
            );
        }
        if validator.errors.is_empty() {
            Ok(())
        } else {
            Err(validator.errors)
        }
    }

    #[test]
    fn enum_input_requires_a_statically_constrained_source_subset() {
        let unrestricted = validate_interpreter_source(FieldSchema::required(ContextType::STRING));
        assert!(has_error(&unrestricted, |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidBindingValue { node, field, message }
                if node == "script"
                    && field == "interpreter"
                    && message.contains("not constrained")
        )));

        let constrained = validate_interpreter_source(
            FieldSchema::required(ContextType::STRING).with_allowed_values(["sh", "bash"]),
        );
        assert!(constrained.is_ok(), "{constrained:?}");

        let incompatible = validate_interpreter_source(
            FieldSchema::required(ContextType::STRING).with_allowed_values(["sh", "python"]),
        );
        assert!(has_error(&incompatible, |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidBindingValue { node, field, .. }
                if node == "script" && field == "interpreter"
        )));
    }

    #[test]
    fn validates_linear_graph_and_dominating_context_reference() {
        let graph = graph(
            &["list"],
            vec![
                action("list"),
                identifier_action_with_ref(
                    "clone",
                    FieldRef::step("list")
                        .field("github")
                        .field("account")
                        .field("login"),
                ),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "clone")],
        );
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn required_input_accepts_a_required_leaf_at_a_positional_index() {
        let graph = graph(
            &["list"],
            vec![
                action("list"),
                action_with_ref(
                    "inspect",
                    FieldRef::step("list")
                        .field("github")
                        .field("repositories")
                        .index(2)
                        .field("https_url"),
                ),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "inspect")],
        );

        assert!(graph.validate().is_ok());
    }

    #[test]
    fn nullable_leaf_at_a_positional_index_remains_invalid_for_required_input() {
        let mut fetch = step("fetch");
        fetch.action = Action::GitFetch {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: "main".into(),
        };
        let graph = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::Action(Box::new(ActionNode {
                    step: fetch,
                    bindings: BTreeMap::from([(
                        "branch".into(),
                        Binding::field(
                            FieldRef::step("list")
                                .field("github")
                                .field("repositories")
                                .index(2)
                                .field("default_branch"),
                        ),
                    )]),
                })),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "fetch")],
        );

        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::BindingMayBeMissing { node, field }
                if node == "fetch" && field == "branch"
        )));
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::BindingMayBeNull { node, field }
                if node == "fetch" && field == "branch"
        )));
    }

    #[test]
    fn linear_lowering_moves_step_bindings_to_the_action_node() {
        let producer = step("list");
        let mut consumer = step("inspect");
        consumer.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        let expected = Binding::field(
            FieldRef::step("list")
                .field("github")
                .field("repositories")
                .index(2)
                .field("https_url"),
        );
        consumer.bindings.insert("repo".into(), expected.clone());

        let graph = LegacyTaskImporter::import_steps(&[producer, consumer]).unwrap();
        let GraphNode::Action(consumer) = &graph.nodes[1] else {
            panic!("expected lowered action node")
        };
        assert!(consumer.step.bindings.is_empty());
        assert_eq!(consumer.bindings.get("repo"), Some(&expected));
    }

    #[test]
    fn linear_lowering_rejects_unknown_and_duplicate_normalized_targets() {
        let mut unknown = step("unknown");
        unknown.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        unknown.bindings.insert(
            "repository_url".into(),
            Binding::literal("https://github.com/example/other.git"),
        );
        let error = LegacyTaskImporter::import_steps(&[unknown]).unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::InvalidGeneratedGraph { ref errors }
                if errors.iter().any(|error| matches!(
                    error.kind,
                    GraphValidationErrorKind::UnknownBindingField { .. }
                ))
        ));

        let mut duplicate = step("duplicate");
        duplicate.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        duplicate.bindings = BTreeMap::from([
            (
                "repo".into(),
                Binding::literal("https://github.com/example/one.git"),
            ),
            (
                "/repo".into(),
                Binding::literal("https://github.com/example/two.git"),
            ),
        ]);
        let error = LegacyTaskImporter::import_steps(&[duplicate]).unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::InvalidGeneratedGraph { ref errors }
                if errors.iter().any(|error| matches!(
                    error.kind,
                    GraphValidationErrorKind::DuplicateBindingTarget { .. }
                ))
        ));
    }

    #[test]
    fn rejects_context_from_non_dominating_parallel_path() {
        let graph = graph(
            &["left", "right"],
            vec![
                action("left"),
                action("right"),
                identifier_action_with_ref(
                    "merge",
                    FieldRef::step("left")
                        .field("github")
                        .field("account")
                        .field("login"),
                ),
            ],
            vec![
                GraphEdge::new("left", EdgePort::Success, "merge"),
                GraphEdge::new("right", EdgePort::Success, "merge"),
            ],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::ContextNotVisible { consumer, producer }
                if consumer == "merge" && producer == "left"
        )));
    }

    #[test]
    fn validates_unicode_alias_and_loop_context_in_nested_body() {
        let body_action = action_with_ref(
            "клонировать",
            FieldRef::loop_item("цикл").field("https_url"),
        );
        let condition = ExpressionV1::Exists {
            reference: ReferenceV1::Local {
                binding: "репозиторий".into(),
                path: vec!["https_url".into()],
            },
        };
        let nested_if = GraphNode::If(IfNode {
            id: "проверка".into(),
            condition,
            then_graph: Box::new(graph(&["выполнить"], vec![action("выполнить")], vec![])),
            else_graph: None,
        });
        let body = graph(
            &["клонировать"],
            vec![body_action, nested_if],
            vec![GraphEdge::new("клонировать", EdgePort::Success, "проверка")],
        );
        let root = graph(
            &["источник"],
            vec![
                action("источник"),
                GraphNode::ForEach(ForEachNode {
                    id: "цикл".into(),
                    collection: Binding::field(
                        FieldRef::step("источник")
                            .field("github")
                            .field("repositories"),
                    ),
                    item_alias: "репозиторий".into(),
                    index_alias: Some("индекс".into()),
                    concurrency: 4,
                    on_error: LoopFailurePolicy::Continue,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("источник", EdgePort::Success, "цикл")],
        );
        assert!(root.validate().is_ok());
    }

    #[test]
    fn rejects_loop_context_outside_its_body() {
        let graph = graph(
            &["consumer"],
            vec![action_with_ref(
                "consumer",
                FieldRef::loop_item("loop").field("name"),
            )],
            vec![],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::LoopContextNotVisible { loop_node, .. }
                if loop_node == "loop"
        )));
    }

    #[test]
    fn sibling_branch_outputs_do_not_leak() {
        let then_graph = graph(&["then-step"], vec![action("then-step")], vec![]);
        let else_graph = graph(
            &["else-step"],
            vec![identifier_action_with_ref(
                "else-step",
                FieldRef::step("then-step")
                    .field("github")
                    .field("account")
                    .field("login"),
            )],
            vec![],
        );
        let graph = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition: ExpressionV1::Literal {
                    value: ExpressionValue::Bool(true),
                },
                then_graph: Box::new(then_graph),
                else_graph: Some(Box::new(else_graph)),
            })],
            vec![],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::ContextNotVisible { consumer, producer }
                if consumer == "else-step" && producer == "then-step"
        )));
    }

    #[test]
    fn reports_duplicate_node_ids_across_nested_graphs() {
        let graph = graph(
            &["loop"],
            vec![GraphNode::ForEach(ForEachNode {
                id: "loop".into(),
                collection: Binding::literal(Vec::<Value>::new()),
                item_alias: "item".into(),
                index_alias: None,
                concurrency: 1,
                on_error: LoopFailurePolicy::Stop,
                body: Box::new(graph(&["loop"], vec![action("loop")], vec![])),
            })],
            vec![],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::DuplicateNodeId { id, .. } if id == "loop"
        )));
    }

    #[test]
    fn detects_cycles_unknown_endpoints_and_invalid_ports() {
        let graph = graph(
            &["a"],
            vec![action("a"), action("b")],
            vec![
                GraphEdge::new("a", EdgePort::Success, "b"),
                GraphEdge::new("b", EdgePort::Success, "a"),
                GraphEdge {
                    from: EdgeEndpoint::output("missing", EdgePort::Success),
                    to: EdgeEndpoint::output("b", EdgePort::Failure),
                },
            ],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::Cycle { .. }
        )));
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::UnknownEndpoint { node } if node == "missing"
        )));
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidTargetPort { node, .. } if node == "b"
        )));
    }

    #[test]
    fn join_requires_at_least_two_incoming_paths() {
        let graph = graph(
            &["source"],
            vec![
                action("source"),
                GraphNode::Join(JoinNode {
                    id: "join".into(),
                    mode: JoinMode::All,
                }),
            ],
            vec![GraphEdge::new("source", EdgePort::Success, "join")],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::JoinNeedsMultipleInputs { node, found: 1 }
                if node == "join"
        )));
    }

    #[test]
    fn switch_rejects_duplicate_case_ids_and_values() {
        let case = |id: &str| SwitchCase {
            id: id.into(),
            values: vec![Value::String("main".into())],
            graph: Box::new(graph(
                &[&format!("{id}-step")],
                vec![action(&format!("{id}-step"))],
                vec![],
            )),
        };
        let graph = graph(
            &["switch"],
            vec![GraphNode::Switch(SwitchNode {
                id: "switch".into(),
                selector: Binding::literal("main"),
                cases: vec![case("same"), case("same")],
                default: None,
            })],
            vec![],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::DuplicateSwitchCaseId { .. }
        )));
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::DuplicateSwitchValue { .. }
        )));
    }

    #[test]
    fn explicit_exits_must_cover_terminal_nodes() {
        let mut graph = graph(
            &["source"],
            vec![action("source"), action("terminal")],
            vec![GraphEdge::new("source", EdgePort::Success, "terminal")],
        );
        graph.exits.push(GraphExit {
            name: "done".into(),
            from: EdgeEndpoint::output("source", EdgePort::Failure),
        });
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::UnlistedTerminal { node } if node == "terminal"
        )));
    }

    #[test]
    fn missing_wire_version_defaults_to_v3_and_unicode_round_trips() {
        let json = serde_json::json!({
            "id": "граф",
            "entries": ["шаг"],
            "nodes": [{
                "kind": "action",
                "config": {
                    "step": {
                        "id": "шаг",
                        "name": "Получить репозитории",
                        "type": "github-list-repositories"
                    }
                }
            }],
            "edges": []
        });
        let graph: WorkflowGraph = serde_json::from_value(json).unwrap();
        assert_eq!(graph.version, WORKFLOW_GRAPH_VERSION);
        assert!(graph.validate().is_ok());
        let round_trip: WorkflowGraph =
            serde_json::from_value(serde_json::to_value(&graph).unwrap()).unwrap();
        assert_eq!(round_trip.id.as_deref(), Some("граф"));
        assert_eq!(round_trip.nodes[0].id(), "шаг");
    }

    #[test]
    fn explicit_v2_graph_migrates_nested_graphs_recursively() {
        let mut branch = graph(&["inside"], vec![action("inside")], vec![]);
        branch.version = 2;
        let mut root = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition: ExpressionV1::Literal {
                    value: ExpressionValue::Bool(true),
                },
                then_graph: Box::new(branch),
                else_graph: None,
            })],
            vec![],
        );
        root.version = 2;

        let migrated = root.into_v3().unwrap();
        assert_eq!(migrated.version, WORKFLOW_GRAPH_VERSION);
        let GraphNode::If(choice) = &migrated.nodes[0] else {
            panic!("expected if node")
        };
        assert_eq!(choice.then_graph.version, WORKFLOW_GRAPH_VERSION);
        migrated.validate().unwrap();
    }

    #[test]
    fn graph_migration_rejects_unknown_future_versions() {
        let mut root = graph(&["step"], vec![action("step")], vec![]);
        root.version = WORKFLOW_GRAPH_VERSION + 1;
        let error = root.into_v3().unwrap_err();
        assert!(matches!(
            error,
            WorkflowGraphMigrationError::UnsupportedVersion { found, .. }
                if found == WORKFLOW_GRAPH_VERSION + 1
        ));
    }

    #[test]
    fn legacy_foreach_action_is_rejected_inside_v2_graph() {
        let mut legacy = step("legacy");
        legacy.action = Action::ForEach {
            source_step: "source".into(),
            array_path: "items".into(),
            item: "item".into(),
            fields: vec![],
        };
        let graph = graph(
            &["source"],
            vec![
                action("source"),
                GraphNode::Action(Box::new(ActionNode {
                    step: legacy,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new("source", EdgePort::Success, "legacy")],
        );
        let result = graph.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::LegacyControlAction { node } if node == "legacy"
        )));
    }

    #[test]
    fn action_binding_target_must_exist_in_block_input_schema() {
        let node = GraphNode::Action(Box::new(ActionNode {
            step: step("list"),
            bindings: BTreeMap::from([("repository_url".into(), Binding::literal("x"))]),
        }));
        let result = graph(&["list"], vec![node], vec![]).validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::UnknownBindingField { node, field }
                if node == "list" && field == "repository_url"
        )));
    }

    #[test]
    fn direct_field_binding_checks_semantic_format() {
        let root = graph(
            &["list"],
            vec![
                action("list"),
                action_with_ref(
                    "clone",
                    FieldRef::step("list")
                        .field("github")
                        .field("account")
                        .field("login"),
                ),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "clone")],
        );
        let result = root.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::BindingTypeMismatch { node, field, .. }
                if node == "clone" && field == "repo"
        )));
    }

    #[test]
    fn foreach_collection_must_be_statically_an_array() {
        let body = graph(&["inside"], vec![action("inside")], vec![]);
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::ForEach(ForEachNode {
                    id: "loop".into(),
                    collection: Binding::field(
                        FieldRef::step("list").field("github").field("account"),
                    ),
                    item_alias: "repository".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "loop")],
        );
        let result = root.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::ForEachCollectionNotArray { node, .. }
                if node == "loop"
        )));
    }

    #[test]
    fn interpolated_fields_must_be_visible_scalars() {
        let mut consumer = step("directory");
        consumer.action = Action::CreateDirectory(crate::automation::task::CreateDirectoryAction {
            path: "/tmp/example".into(),
        });
        let node = GraphNode::Action(Box::new(ActionNode {
            step: consumer,
            bindings: BTreeMap::from([(
                "path".into(),
                Binding::interpolated([
                    TemplatePart::literal("/tmp/"),
                    TemplatePart::field(FieldRef::step("list").field("github").field("account")),
                ]),
            )]),
        }));
        let root = graph(
            &["list"],
            vec![action("list"), node],
            vec![GraphEdge::new("list", EdgePort::Success, "directory")],
        );
        let result = root.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::InterpolatedFieldNotScalar { consumer, .. }
                if consumer == "directory"
        )));
    }

    #[test]
    fn if_rule_is_type_checked_against_dominating_schema() {
        let choice = GraphNode::If(IfNode {
            id: "choice".into(),
            condition: ExpressionV1::Ref {
                reference: ReferenceV1::Context {
                    field: FieldRef::step("list")
                        .field("github")
                        .field("account")
                        .field("login"),
                },
            },
            then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
            else_graph: None,
        });
        let root = graph(
            &["list"],
            vec![action("list"), choice],
            vec![GraphEdge::new("list", EdgePort::Success, "choice")],
        );

        let result = root.validate();
        assert!(has_error(&result, |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "ExpectedBoolean"
        )));
    }

    #[test]
    fn expression_rejects_unknown_and_non_dominating_fields() {
        let unknown = GraphNode::If(IfNode {
            id: "unknown".into(),
            condition: ExpressionV1::Exists {
                reference: ReferenceV1::Context {
                    field: FieldRef::step("list")
                        .field("github")
                        .field("does_not_exist"),
                },
            },
            then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
            else_graph: None,
        });
        let unknown_graph = graph(
            &["list"],
            vec![action("list"), unknown],
            vec![GraphEdge::new("list", EdgePort::Success, "unknown")],
        );
        assert!(has_error(&unknown_graph.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "UnknownReference"
        )));

        let parallel = graph(
            &["left", "choice"],
            vec![
                action("left"),
                GraphNode::If(IfNode {
                    id: "choice".into(),
                    condition: ExpressionV1::Exists {
                        reference: ReferenceV1::Context {
                            field: FieldRef::step("left")
                                .field("github")
                                .field("account")
                                .field("login"),
                        },
                    },
                    then_graph: Box::new(graph(
                        &["parallel-inside"],
                        vec![action("parallel-inside")],
                        vec![],
                    )),
                    else_graph: None,
                }),
            ],
            vec![],
        );
        assert!(has_error(&parallel.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "UnknownReference"
        )));
    }

    #[test]
    fn expression_rejects_undeclared_scenario_root() {
        let root = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition: ExpressionV1::Exists {
                    reference: ReferenceV1::Context {
                        field: FieldRef::scenario().field("undeclared"),
                    },
                },
                then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
                else_graph: None,
            })],
            vec![],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "UnknownReference"
        )));
    }

    #[test]
    fn action_when_expression_uses_same_static_checker() {
        let mut consumer = step("consumer");
        consumer.when = Some(StepCondition::Expression {
            rule: ExpressionV1::Ref {
                reference: ReferenceV1::Context {
                    field: FieldRef::step("list")
                        .field("github")
                        .field("account")
                        .field("login"),
                },
            },
            policy: RuleOutcomePolicy::default(),
        });
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::Action(Box::new(ActionNode {
                    step: consumer,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "consumer")],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "ExpectedBoolean"
        )));
    }

    #[test]
    fn loop_alias_is_rewritten_with_its_item_type() {
        let typed_condition = ExpressionV1::Contains {
            value: Box::new(ExpressionV1::Ref {
                reference: ReferenceV1::Local {
                    binding: "repository".into(),
                    path: vec!["private".into()],
                },
            }),
            needle: Box::new(ExpressionV1::Literal {
                value: ExpressionValue::String("yes".into()),
            }),
        };
        let body = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition: typed_condition,
                then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
                else_graph: None,
            })],
            vec![],
        );
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::ForEach(ForEachNode {
                    id: "loop".into(),
                    collection: Binding::field(
                        FieldRef::step("list").field("github").field("repositories"),
                    ),
                    item_alias: "repository".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "loop")],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidExpression { code, .. }
                if code == "TypeMismatch"
        )));
    }

    #[test]
    fn quantifier_cannot_shadow_graph_loop_alias() {
        let condition = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(ExpressionV1::Literal {
                value: ExpressionValue::List(vec![ExpressionValue::Bool(true)]),
            }),
            binding: "repository".into(),
            predicate: Box::new(ExpressionV1::Ref {
                reference: ReferenceV1::Local {
                    binding: "repository".into(),
                    path: Vec::new(),
                },
            }),
        };
        let body = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition,
                then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
                else_graph: None,
            })],
            vec![],
        );
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::ForEach(ForEachNode {
                    id: "loop".into(),
                    collection: Binding::field(
                        FieldRef::step("list").field("github").field("repositories"),
                    ),
                    item_alias: "repository".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "loop")],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ShadowedAlias { alias, .. }
                if alias == "repository"
        )));
    }

    #[test]
    fn secret_expression_field_is_rejected_by_schema_resolver() {
        let secret_schema = ObjectSchema::new("test.secret@1").with_field(
            "token",
            FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Secret),
        );
        let mut validator = GraphValidator::default();
        validator
            .global_output_schemas
            .insert("source".into(), secret_schema);
        validator
            .global_ids
            .insert("source".into(), "graph.source".into());
        let local_ids = BTreeSet::from(["source".into(), "consumer".into()]);
        let dominators = BTreeMap::from([
            ("source".into(), BTreeSet::from(["source".into()])),
            (
                "consumer".into(),
                BTreeSet::from(["source".into(), "consumer".into()]),
            ),
        ]);
        validator.validate_checked_expression(
            &ExpressionV1::Exists {
                reference: ReferenceV1::Context {
                    field: FieldRef::step("source").field("token"),
                },
            },
            "consumer",
            "graph.consumer.condition",
            &local_ids,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &dominators,
        );
        assert!(validator.errors.iter().any(|error| matches!(
            &error.kind,
            GraphValidationErrorKind::InvalidExpression { message, .. }
                if message.contains("secret context field")
        )));
    }

    #[test]
    fn literal_binding_is_checked_against_semantic_format() {
        let mut clone = step("clone");
        clone.action = Action::GitClone {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: None,
        };
        let root = graph(
            &["clone"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: clone,
                bindings: BTreeMap::from([("repo".into(), Binding::literal("not a git url"))]),
            }))],
            vec![],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::InvalidBindingValue { node, field, .. }
                if node == "clone" && field == "repo"
        )));
    }

    #[test]
    fn valid_formatted_literal_binding_is_accepted() {
        let mut clone = step("clone");
        clone.action = Action::GitClone {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: None,
        };
        let root = graph(
            &["clone"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: clone,
                bindings: BTreeMap::from([(
                    "repo".into(),
                    Binding::literal("https://github.com/example/other.git"),
                )]),
            }))],
            vec![],
        );
        assert!(root.validate().is_ok());
    }

    #[test]
    fn formatted_input_allows_structural_interpolation_when_value_is_dynamic() {
        let mut clone = step("clone");
        clone.action = Action::GitClone {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: None,
        };
        let body = graph(
            &["clone"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: clone,
                bindings: BTreeMap::from([(
                    "repo".into(),
                    Binding::interpolated([TemplatePart::field(
                        FieldRef::loop_item("loop").field("https_url"),
                    )]),
                )]),
            }))],
            vec![],
        );
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::ForEach(ForEachNode {
                    id: "loop".into(),
                    collection: Binding::field(
                        FieldRef::step("list").field("github").field("repositories"),
                    ),
                    item_alias: "repository".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "loop")],
        );
        assert!(root.validate().is_ok());
    }

    #[test]
    fn optional_source_can_omit_an_optional_input_override() {
        let mut clone = step("clone");
        clone.action = Action::GitClone {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
            branch: None,
        };
        let body = graph(
            &["clone"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: clone,
                bindings: BTreeMap::from([(
                    "branch".into(),
                    Binding::field(FieldRef::loop_item("loop").field("default_branch")),
                )]),
            }))],
            vec![],
        );
        let root = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::ForEach(ForEachNode {
                    id: "loop".into(),
                    collection: Binding::field(
                        FieldRef::step("list").field("github").field("repositories"),
                    ),
                    item_alias: "repository".into(),
                    index_alias: None,
                    concurrency: 1,
                    on_error: LoopFailurePolicy::Stop,
                    body: Box::new(body),
                }),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "loop")],
        );

        // Runtime binding materialization treats a missing source as "omit"
        // only when the destination input is optional, preserving the action
        // literal/default. Required destinations remain fail-closed.
        root.validate().unwrap();
    }

    #[test]
    fn undeclared_scenario_binding_is_rejected_statically() {
        let clone = default_step(ActionKind::GitCloneIfMissing, "clone").unwrap();
        let root = graph(
            &["clone"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: clone,
                bindings: BTreeMap::from([(
                    "branch".into(),
                    Binding::field(FieldRef::scenario().field("branch")),
                )]),
            }))],
            vec![],
        );

        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::UnknownContextField { producer, field, .. }
                if producer == "scenario input" && field == "branch"
        )));
    }

    #[test]
    fn indexed_action_input_target_is_rejected_statically() {
        let run = default_step(ActionKind::RunCommand, "run").unwrap();
        let root = graph(
            &["run"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: run,
                bindings: BTreeMap::from([("args.0".into(), Binding::literal("--version"))]),
            }))],
            vec![],
        );

        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::UnknownBindingField { node, field }
                if node == "run" && field == "args.0"
        )));
    }

    #[test]
    fn switch_cases_must_match_selector_type() {
        let case = SwitchCase {
            id: "text".into(),
            values: vec![Value::String("one".into())],
            graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
        };
        let root = graph(
            &["switch"],
            vec![GraphNode::Switch(SwitchNode {
                id: "switch".into(),
                selector: Binding::literal(1),
                cases: vec![case],
                default: None,
            })],
            vec![],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::SwitchCaseTypeMismatch { node, .. }
                if node == "switch"
        )));
    }

    #[test]
    fn exit_code_condition_requires_run_script_and_success_code() {
        let mut wrong_consumer = step("wrong-consumer");
        wrong_consumer.when = Some(StepCondition::ExitCode {
            step: "list".into(),
            codes: vec![0],
        });
        let wrong_source = graph(
            &["list"],
            vec![
                action("list"),
                GraphNode::Action(Box::new(ActionNode {
                    step: wrong_consumer,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new("list", EdgePort::Success, "wrong-consumer")],
        );
        assert!(has_error(&wrong_source.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ExitCodeSourceNotRunScript { producer, .. }
                if producer == "list"
        )));

        let mut script = step("script");
        script.dangerous = true;
        script.action = Action::RunScript {
            interpreter: ScriptInterpreter::Bash,
            script: "scripts/check.sh".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            success_exit_codes: vec![0],
        };
        let mut consumer = step("consumer");
        consumer.when = Some(StepCondition::ExitCode {
            step: "script".into(),
            codes: vec![1],
        });
        let invalid_code = graph(
            &["script"],
            vec![
                GraphNode::Action(Box::new(ActionNode {
                    step: script,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::Action(Box::new(ActionNode {
                    step: consumer,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new("script", EdgePort::Success, "consumer")],
        );
        assert!(has_error(&invalid_code.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ExitCodeNotSuccessful { producer, code: 1, .. }
                if producer == "script"
        )));
    }

    #[test]
    fn exit_code_preflight_accepts_bounded_lists() {
        let codes = (0..GRAPH_MAX_EXIT_CODES_PER_LIST as u32).collect::<Vec<_>>();
        let mut script = step("script");
        script.dangerous = true;
        script.action = Action::RunScript {
            interpreter: ScriptInterpreter::Bash,
            script: "scripts/check.sh".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            success_exit_codes: codes.clone(),
        };
        let mut consumer = step("consumer");
        consumer.when = Some(StepCondition::ExitCode {
            step: "script".into(),
            codes,
        });
        let root = graph(
            &["script"],
            vec![
                GraphNode::Action(Box::new(ActionNode {
                    step: script,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::Action(Box::new(ActionNode {
                    step: consumer,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new("script", EdgePort::Success, "consumer")],
        );

        assert!(root.validate().is_ok());
    }

    #[test]
    fn exit_code_preflight_rejects_oversized_lists() {
        let mut script = step("script");
        script.dangerous = true;
        script.action = Action::RunScript {
            interpreter: ScriptInterpreter::Bash,
            script: "scripts/check.sh".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            success_exit_codes: vec![0; GRAPH_MAX_EXIT_CODES_PER_LIST + 1],
        };
        let oversized_source = graph(
            &["script"],
            vec![GraphNode::Action(Box::new(ActionNode {
                step: script,
                bindings: BTreeMap::new(),
            }))],
            vec![],
        );
        assert!(has_error(&oversized_source.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ResourceLimitExceeded { resource, .. }
                if resource == "exit codes in one list"
        )));

        let mut bounded_script = step("bounded-script");
        bounded_script.dangerous = true;
        bounded_script.action = Action::RunScript {
            interpreter: ScriptInterpreter::Bash,
            script: "scripts/check.sh".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            success_exit_codes: vec![0],
        };
        let mut consumer = step("consumer");
        consumer.when = Some(StepCondition::ExitCode {
            step: "bounded-script".into(),
            codes: vec![0; GRAPH_MAX_EXIT_CODES_PER_LIST + 1],
        });
        let oversized_condition = graph(
            &["bounded-script"],
            vec![
                GraphNode::Action(Box::new(ActionNode {
                    step: bounded_script,
                    bindings: BTreeMap::new(),
                })),
                GraphNode::Action(Box::new(ActionNode {
                    step: consumer,
                    bindings: BTreeMap::new(),
                })),
            ],
            vec![GraphEdge::new(
                "bounded-script",
                EdgePort::Success,
                "consumer",
            )],
        );
        assert!(has_error(&oversized_condition.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ResourceLimitExceeded { resource, .. }
                if resource == "exit codes in one list"
        )));
    }

    #[test]
    fn aggregate_payload_budget_rejects_first_byte_over_limit() {
        let mut validator = GraphValidator::default();
        let mut budget = GraphPreflightBudget::default();
        assert!(validator.preflight_payload_bytes(
            GRAPH_MAX_TOTAL_PAYLOAD_BYTES,
            "graph",
            &mut budget,
        ));
        assert!(!validator.preflight_payload_bytes(1, "graph", &mut budget));
        assert!(validator.errors.iter().any(|error| matches!(
            error.kind,
            GraphValidationErrorKind::ResourceLimitExceeded {
                ref resource,
                found,
                limit,
            } if resource == "total graph payload bytes"
                && found == GRAPH_MAX_TOTAL_PAYLOAD_BYTES + 1
                && limit == GRAPH_MAX_TOTAL_PAYLOAD_BYTES
        )));
    }

    #[test]
    fn preflight_rejects_wide_expressions_before_cloning_them() {
        let condition = ExpressionV1::All {
            expressions: (0..=ExpressionLimits::default().max_nodes)
                .map(|_| ExpressionV1::Literal {
                    value: ExpressionValue::Bool(true),
                })
                .collect(),
        };
        let root = graph(
            &["choice"],
            vec![GraphNode::If(IfNode {
                id: "choice".into(),
                condition,
                then_graph: Box::new(graph(&["inside"], vec![action("inside")], vec![])),
                else_graph: None,
            })],
            vec![],
        );
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ResourceLimitExceeded { resource, .. }
                if resource == "expression nodes"
        )));
    }

    #[test]
    fn reserved_loop_index_scope_cannot_be_a_real_node_id() {
        let root = graph(&["loop::index"], vec![action("loop::index")], vec![]);
        assert!(has_error(&root.validate(), |kind| matches!(
            kind,
            GraphValidationErrorKind::ReservedNodeId { id } if id == "loop::index"
        )));
    }

    #[test]
    fn linear_v1_migration_preserves_slice_order_only() {
        let steps = vec![step("first"), step("second"), step("third")];
        let migrated = LegacyTaskImporter::import_steps(&steps).unwrap();

        assert_eq!(migrated.entries, vec!["first"]);
        assert_eq!(
            migrated.nodes.iter().map(GraphNode::id).collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(
            migrated.edges,
            vec![
                GraphEdge::new("first", EdgePort::Success, "second"),
                GraphEdge::new("second", EdgePort::Success, "third"),
            ]
        );
    }

    #[test]
    fn proven_github_foreach_clone_pair_migrates_to_nested_graph() {
        let source = step("repositories");
        let mut loop_step = step("repositories-loop");
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec![
                "https_url".into(),
                "owner".into(),
                "name".into(),
                "default_branch".into(),
            ],
        };
        let mut clone = step("clone");
        clone.action = Action::ForEachGitCloneIfMissing {
            loop_step: "repositories-loop".into(),
            repo: "{{repository.https_url}}".into(),
            dest: "$HOME/Developer/{{repository.owner}}/{{repository.name}}".into(),
            branch: None,
        };

        let migrated = LegacyTaskImporter::import_steps(&[source, loop_step, clone]).unwrap();
        assert_eq!(migrated.nodes.len(), 2);
        assert_eq!(
            migrated.edges,
            vec![GraphEdge::new(
                "repositories",
                EdgePort::Success,
                "repositories-loop"
            )]
        );
        let GraphNode::ForEach(control) = &migrated.nodes[1] else {
            panic!("expected migrated for-each control node")
        };
        assert_eq!(control.item_alias, "repository");
        assert_eq!(control.body.entries, vec!["clone"]);
        let GraphNode::Action(action) = &control.body.nodes[0] else {
            panic!("expected nested clone action")
        };
        assert!(matches!(
            action.step.action,
            Action::GitCloneIfMissing { .. }
        ));
        assert!(matches!(
            action.bindings.get("repo"),
            Some(Binding::Field { .. })
        ));
        assert!(matches!(
            action.bindings.get("dest"),
            Some(Binding::Interpolated { .. })
        ));
        assert!(!action.bindings.contains_key("branch"));
    }

    #[test]
    fn explicit_loop_item_consumer_migrates_to_generic_foreach_body() {
        let source = step("repositories");
        let mut loop_step = step("repositories-loop");
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into(), "name".into()],
        };
        let mut inspect = step("inspect");
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        let item_binding =
            Binding::field(FieldRef::loop_item("repositories-loop").field("https_url"));
        inspect.bindings.insert("repo".into(), item_binding.clone());

        let migrated = LegacyTaskImporter::import_steps(&[source, loop_step, inspect]).unwrap();

        assert_eq!(migrated.nodes.len(), 2);
        assert_eq!(
            migrated.edges,
            vec![GraphEdge::new(
                "repositories",
                EdgePort::Success,
                "repositories-loop"
            )]
        );
        let GraphNode::ForEach(control) = &migrated.nodes[1] else {
            panic!("expected migrated for-each control node")
        };
        assert_eq!(control.body.entries, vec!["inspect"]);
        let GraphNode::Action(action) = &control.body.nodes[0] else {
            panic!("expected generic action in loop body")
        };
        assert!(action.step.bindings.is_empty());
        assert_eq!(action.bindings.get("repo"), Some(&item_binding));
    }

    #[test]
    fn positional_array_consumer_does_not_imply_foreach_membership() {
        let source = step("repositories");
        let mut loop_step = step("repositories-loop");
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec![],
        };
        let mut inspect = step("inspect");
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(
                FieldRef::step("repositories")
                    .field("github")
                    .field("repositories")
                    .index(2)
                    .field("https_url"),
            ),
        );

        let error = LegacyTaskImporter::import_steps(&[source, loop_step, inspect]).unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::UnsupportedLegacyControl { step, reason }
                if step == "repositories-loop" && reason.contains("does not structurally reference")
        ));
    }

    #[test]
    fn generic_foreach_consumer_cannot_escape_legacy_field_projection() {
        let source = step("repositories");
        let mut loop_step = step("repositories-loop");
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec!["https_url".into()],
        };
        let mut inspect = step("inspect");
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(FieldRef::loop_item("repositories-loop").field("ssh_url")),
        );

        let error = LegacyTaskImporter::import_steps(&[source, loop_step, inspect]).unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::UnsupportedLegacyControl { step, reason }
                if step == "repositories-loop" && reason.contains("omitted by the v1 projection")
        ));
    }

    #[test]
    fn legacy_generic_foreach_rejects_loop_item_conditions_until_graph_lowering() {
        let source = step("repositories");
        let mut loop_step = step("repositories-loop");
        loop_step.action = Action::ForEach {
            source_step: "repositories".into(),
            array_path: "github.repositories".into(),
            item: "repository".into(),
            fields: vec![],
        };
        let mut inspect = step("inspect");
        inspect.action = Action::GitInspect {
            repo: "https://github.com/example/repository.git".into(),
            dest: "/tmp/repository".into(),
        };
        inspect.bindings.insert(
            "repo".into(),
            Binding::field(FieldRef::loop_item("repositories-loop").field("https_url")),
        );
        inspect.when = Some(StepCondition::Expression {
            rule: ExpressionV1::Exists {
                reference: ReferenceV1::Context {
                    field: FieldRef::loop_item("repositories-loop").field("name"),
                },
            },
            policy: RuleOutcomePolicy::default(),
        });

        let error = LegacyTaskImporter::import_steps(&[source, loop_step, inspect]).unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::UnsupportedLegacyControl { step, reason }
                if step == "inspect" && reason.contains("explicit v2 graph")
        ));
    }

    #[test]
    fn ambiguous_legacy_control_shape_is_not_guessed() {
        let mut loop_step = step("loop");
        loop_step.action = Action::ForEach {
            source_step: "source".into(),
            array_path: "items".into(),
            item: "item".into(),
            fields: vec![],
        };
        let error = LegacyTaskImporter::import_steps(&[step("source"), loop_step, step("other")])
            .unwrap_err();
        assert!(matches!(
            error,
            LinearMigrationError::UnsupportedLegacyControl { step, .. } if step == "loop"
        ));
    }
}
