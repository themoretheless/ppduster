//! Typed, side-effect-free expressions for scenario conditions.
//!
//! The serialized [`ExpressionV1`] tree is deliberately independent from an
//! interpreter implementation. A tree must be checked against an
//! [`ExpressionSchemaResolver`] before it can be evaluated. Evaluation has no
//! ambient capabilities: the only data it can observe is supplied through an
//! [`ExpressionValueResolver`], and all potentially expensive work is bounded
//! by [`ExpressionLimits`].

use std::collections::BTreeMap;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use super::context::{
    ContextPathSegment, ContextScope, ContextStore, ContextType, ContextValue, FieldRef,
    ObjectSchema, Sensitivity, CONTEXT_SCHEMA_VERSION,
};

/// Version-one expression AST used by both value expressions and Boolean rules.
///
/// `check_rule` requires the root expression to have Boolean type. Keeping one
/// AST avoids two subtly different implementations of references, nullability,
/// collection predicates, and limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExpressionV1 {
    Literal {
        value: ExpressionValue,
    },
    Ref {
        reference: ReferenceV1,
    },
    All {
        expressions: Vec<ExpressionV1>,
    },
    Any {
        expressions: Vec<ExpressionV1>,
    },
    Not {
        expression: Box<ExpressionV1>,
    },
    Exists {
        reference: ReferenceV1,
    },
    IsNull {
        expression: Box<ExpressionV1>,
    },
    IsEmpty {
        expression: Box<ExpressionV1>,
    },
    Compare {
        operator: ComparisonOperator,
        left: Box<ExpressionV1>,
        right: Box<ExpressionV1>,
    },
    Contains {
        value: Box<ExpressionV1>,
        needle: Box<ExpressionV1>,
    },
    StartsWith {
        value: Box<ExpressionV1>,
        prefix: Box<ExpressionV1>,
    },
    EndsWith {
        value: Box<ExpressionV1>,
        suffix: Box<ExpressionV1>,
    },
    Matches {
        value: Box<ExpressionV1>,
        pattern: String,
    },
    In {
        needle: Box<ExpressionV1>,
        collection: Box<ExpressionV1>,
    },
    Quantifier {
        quantifier: CollectionQuantifier,
        collection: Box<ExpressionV1>,
        binding: String,
        predicate: Box<ExpressionV1>,
    },
}

impl ExpressionV1 {
    /// Visit every context reference contained in this expression.
    ///
    /// Local quantifier references are deliberately excluded: they are scoped
    /// names, not graph-stable [`FieldRef`] values.
    pub fn visit_context_references(&self, mut visitor: impl FnMut(&FieldRef)) {
        self.visit_context_references_inner(&mut visitor);
    }

    /// Mutably visit every context reference contained in this expression.
    ///
    /// This is intended for structural task rewrites such as prefixing stable
    /// step IDs when a template is instantiated. Re-run [`check_rule`] or
    /// [`check_value_expression`] after rewriting references.
    pub fn visit_context_references_mut(&mut self, mut visitor: impl FnMut(&mut FieldRef)) {
        self.visit_context_references_mut_inner(&mut visitor);
    }

    fn visit_context_references_inner(&self, visitor: &mut impl FnMut(&FieldRef)) {
        match self {
            Self::Literal { .. } => {}
            Self::Ref { reference } | Self::Exists { reference } => {
                reference.visit_context_reference(visitor);
            }
            Self::All { expressions } | Self::Any { expressions } => {
                for expression in expressions {
                    expression.visit_context_references_inner(visitor);
                }
            }
            Self::Not { expression }
            | Self::IsNull { expression }
            | Self::IsEmpty { expression } => {
                expression.visit_context_references_inner(visitor);
            }
            Self::Compare { left, right, .. } => {
                left.visit_context_references_inner(visitor);
                right.visit_context_references_inner(visitor);
            }
            Self::Contains { value, needle } => {
                value.visit_context_references_inner(visitor);
                needle.visit_context_references_inner(visitor);
            }
            Self::StartsWith { value, prefix } => {
                value.visit_context_references_inner(visitor);
                prefix.visit_context_references_inner(visitor);
            }
            Self::EndsWith { value, suffix } => {
                value.visit_context_references_inner(visitor);
                suffix.visit_context_references_inner(visitor);
            }
            Self::Matches { value, .. } => value.visit_context_references_inner(visitor),
            Self::In { needle, collection } => {
                needle.visit_context_references_inner(visitor);
                collection.visit_context_references_inner(visitor);
            }
            Self::Quantifier {
                collection,
                predicate,
                ..
            } => {
                collection.visit_context_references_inner(visitor);
                predicate.visit_context_references_inner(visitor);
            }
        }
    }

    fn visit_context_references_mut_inner(&mut self, visitor: &mut impl FnMut(&mut FieldRef)) {
        match self {
            Self::Literal { .. } => {}
            Self::Ref { reference } | Self::Exists { reference } => {
                reference.visit_context_reference_mut(visitor);
            }
            Self::All { expressions } | Self::Any { expressions } => {
                for expression in expressions {
                    expression.visit_context_references_mut_inner(visitor);
                }
            }
            Self::Not { expression }
            | Self::IsNull { expression }
            | Self::IsEmpty { expression } => {
                expression.visit_context_references_mut_inner(visitor);
            }
            Self::Compare { left, right, .. } => {
                left.visit_context_references_mut_inner(visitor);
                right.visit_context_references_mut_inner(visitor);
            }
            Self::Contains { value, needle } => {
                value.visit_context_references_mut_inner(visitor);
                needle.visit_context_references_mut_inner(visitor);
            }
            Self::StartsWith { value, prefix } => {
                value.visit_context_references_mut_inner(visitor);
                prefix.visit_context_references_mut_inner(visitor);
            }
            Self::EndsWith { value, suffix } => {
                value.visit_context_references_mut_inner(visitor);
                suffix.visit_context_references_mut_inner(visitor);
            }
            Self::Matches { value, .. } => value.visit_context_references_mut_inner(visitor),
            Self::In { needle, collection } => {
                needle.visit_context_references_mut_inner(visitor);
                collection.visit_context_references_mut_inner(visitor);
            }
            Self::Quantifier {
                collection,
                predicate,
                ..
            } => {
                collection.visit_context_references_mut_inner(visitor);
                predicate.visit_context_references_mut_inner(visitor);
            }
        }
    }
}

/// A Boolean rule is a value expression whose checked root type is Boolean.
pub type RuleExprV1 = ExpressionV1;

/// Alias used by action-input and future context-transform integrations.
pub type ValueExprV1 = ExpressionV1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReferenceV1 {
    Context {
        field: FieldRef,
    },
    Local {
        binding: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<String>,
    },
}

impl ReferenceV1 {
    fn visit_context_reference(&self, visitor: &mut impl FnMut(&FieldRef)) {
        if let Self::Context { field } = self {
            visitor(field);
        }
    }

    fn visit_context_reference_mut(&mut self, visitor: &mut impl FnMut(&mut FieldRef)) {
        if let Self::Context { field } = self {
            visitor(field);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollectionQuantifier {
    Any,
    All,
    None,
}

/// Runtime and literal values. Numeric variants are intentionally distinct:
/// ppduster expressions never coerce strings or mix signed, unsigned, and
/// floating-point values implicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum ExpressionValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(String),
    List(Vec<ExpressionValue>),
    Object(BTreeMap<String, ExpressionValue>),
}

impl TryFrom<&serde_json::Value> for ExpressionValue {
    type Error = EvaluationError;

    fn try_from(value: &serde_json::Value) -> Result<Self, Self::Error> {
        expression_value_from_json_bounded(value, ExpressionLimits::default())
    }
}

/// Static type used by the expression checker.
///
/// This is a checked projection of the block context schema. `optional`
/// describes field presence; `nullable` describes an explicitly present null.
/// They are separate so a missing value can never be mistaken for `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpressionType {
    pub kind: ExpressionTypeKind,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum ExpressionTypeKind {
    Never,
    Any,
    Null,
    Bool,
    Integer,
    Number,
    String,
    List(Box<ExpressionType>),
    Object(BTreeMap<String, ExpressionType>),
}

impl ExpressionType {
    pub const fn required(kind: ExpressionTypeKind) -> Self {
        Self {
            kind,
            optional: false,
            nullable: false,
        }
    }

    pub const fn optional(kind: ExpressionTypeKind) -> Self {
        Self {
            kind,
            optional: true,
            nullable: false,
        }
    }

    pub const fn nullable(kind: ExpressionTypeKind) -> Self {
        Self {
            kind,
            optional: false,
            nullable: true,
        }
    }

    pub const fn optional_nullable(kind: ExpressionTypeKind) -> Self {
        Self {
            kind,
            optional: true,
            nullable: true,
        }
    }

    fn bool() -> Self {
        Self::required(ExpressionTypeKind::Bool)
    }

    fn null() -> Self {
        Self::required(ExpressionTypeKind::Null)
    }

    fn list(item: ExpressionType) -> Self {
        Self::required(ExpressionTypeKind::List(Box::new(item)))
    }

    fn object(fields: BTreeMap<String, ExpressionType>) -> Self {
        Self::required(ExpressionTypeKind::Object(fields))
    }

    fn describe(&self) -> String {
        let base = self.kind.describe();
        match (self.optional, self.nullable) {
            (false, false) => base,
            (true, false) => format!("optional {base}"),
            (false, true) => format!("nullable {base}"),
            (true, true) => format!("optional nullable {base}"),
        }
    }
}

impl ExpressionTypeKind {
    fn describe(&self) -> String {
        match self {
            Self::Never => "never".into(),
            Self::Any => "any".into(),
            Self::Null => "null".into(),
            Self::Bool => "bool".into(),
            Self::Integer => "integer".into(),
            Self::Number => "number".into(),
            Self::String => "string".into(),
            Self::List(item) => format!("list<{}>", item.describe()),
            Self::Object(_) => "object".into(),
        }
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaResolutionError {
    pub message: String,
}

impl SchemaResolutionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(format!("{SCHEMA_LIMIT_PREFIX}{}", message.into()))
    }

    pub fn is_limit_exceeded(&self) -> bool {
        self.message.starts_with(SCHEMA_LIMIT_PREFIX)
    }

    fn diagnostic_message(&self) -> &str {
        self.message
            .strip_prefix(SCHEMA_LIMIT_PREFIX)
            .unwrap_or(&self.message)
    }
}

const SCHEMA_LIMIT_PREFIX: &str = "[limit-exceeded] ";

/// Resolves stable context references to their checked expression type.
///
/// The context registry owns graph visibility and schema traversal. The
/// expression layer only consumes the resulting type and therefore cannot
/// accidentally make a future/non-dominating block visible.
pub trait ExpressionSchemaResolver {
    fn resolve(&self, reference: &FieldRef) -> Result<ExpressionType, SchemaResolutionError>;

    /// Bounded variant used by the checker. Existing resolvers remain source
    /// compatible; resolvers backed by recursively structured data should
    /// override this method and enforce the supplied limits before cloning.
    fn resolve_schema_bounded(
        &self,
        reference: &FieldRef,
        _limits: ExpressionLimits,
    ) -> Result<ExpressionType, SchemaResolutionError> {
        self.resolve(reference)
    }
}

/// Supplies one immutable activation value for a checked reference.
pub trait ExpressionValueResolver {
    fn resolve(&self, reference: &FieldRef) -> EvaluationValue;

    /// Bounded variant used by checked evaluation. The default preserves
    /// compatibility for activation implementations that already return a
    /// bounded value.
    fn resolve_value_bounded(
        &self,
        reference: &FieldRef,
        _limits: ExpressionLimits,
    ) -> EvaluationValue {
        self.resolve(reference)
    }
}

impl ExpressionSchemaResolver for ContextStore {
    fn resolve(&self, reference: &FieldRef) -> Result<ExpressionType, SchemaResolutionError> {
        self.resolve_schema_bounded(reference, ExpressionLimits::default())
    }

    fn resolve_schema_bounded(
        &self,
        reference: &FieldRef,
        limits: ExpressionLimits,
    ) -> Result<ExpressionType, SchemaResolutionError> {
        resolve_context_schema_bounded(self, reference, limits)
    }
}

impl ExpressionValueResolver for ContextStore {
    fn resolve(&self, reference: &FieldRef) -> EvaluationValue {
        self.resolve_value_bounded(reference, ExpressionLimits::default())
    }

    fn resolve_value_bounded(
        &self,
        reference: &FieldRef,
        limits: ExpressionLimits,
    ) -> EvaluationValue {
        resolve_context_value_bounded(self, reference, limits)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EvaluationValue {
    Value(ExpressionValue),
    Missing(EvaluationIssue),
    Unknown(EvaluationIssue),
    Error(EvaluationError),
}

/// Result of evaluating a checked Boolean rule.
///
/// No state is coerced to `false`: an explicit JSON `null`, a missing optional
/// field, an unavailable upstream value, and an evaluation failure remain
/// observably different so the runner can apply an intentional policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleEvaluation {
    True,
    False,
    Null,
    Missing(EvaluationIssue),
    Unknown(EvaluationIssue),
    Error(EvaluationError),
}

impl EvaluationValue {
    pub fn value(value: ExpressionValue) -> Self {
        Self::Value(value)
    }

    pub fn missing(message: impl Into<String>) -> Self {
        Self::Missing(EvaluationIssue::new(message))
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::Unknown(EvaluationIssue::new(message))
    }

    pub fn error(kind: EvaluationErrorKind, message: impl Into<String>) -> Self {
        Self::Error(EvaluationError::new(kind, message))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationIssue {
    pub message: String,
}

impl EvaluationIssue {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationError {
    pub kind: EvaluationErrorKind,
    pub message: String,
}

impl EvaluationError {
    pub fn new(kind: EvaluationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationErrorKind {
    ContractViolation,
    InvalidOperand,
    LimitExceeded,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpressionLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_operations: usize,
    pub max_collection_visits: usize,
    pub max_string_bytes: usize,
    pub max_regex_pattern_bytes: usize,
    pub max_regex_input_bytes: usize,
    pub max_regex_compiled_bytes: usize,
    pub max_quantifier_depth: usize,
}

impl Default for ExpressionLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_nodes: 256,
            max_operations: 4_096,
            max_collection_visits: 10_000,
            max_string_bytes: 64 * 1024,
            max_regex_pattern_bytes: 4 * 1024,
            max_regex_input_bytes: 64 * 1024,
            max_regex_compiled_bytes: 1024 * 1024,
            max_quantifier_depth: 4,
        }
    }
}

#[derive(Debug)]
struct TraversalLimit {
    message: String,
}

impl TraversalLimit {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn evaluation_error(self) -> EvaluationError {
        EvaluationError::new(EvaluationErrorKind::LimitExceeded, self.message)
    }

    fn schema_error(self) -> SchemaResolutionError {
        SchemaResolutionError::limit_exceeded(self.message)
    }
}

/// Per-resolution budget applied before cloning any recursive schema or JSON
/// subtree. `max_string_bytes` is both a per-string and cumulative budget.
struct TraversalBudget {
    limits: ExpressionLimits,
    nodes: usize,
    collection_visits: usize,
    string_bytes: usize,
}

impl TraversalBudget {
    fn new(limits: ExpressionLimits) -> Self {
        Self {
            limits,
            nodes: 0,
            collection_visits: 0,
            string_bytes: 0,
        }
    }

    fn visit_node(&mut self, depth: usize) -> Result<(), TraversalLimit> {
        if depth > self.limits.max_depth {
            return Err(TraversalLimit::new(format!(
                "context traversal exceeds depth limit {}",
                self.limits.max_depth
            )));
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.limits.max_nodes {
            return Err(TraversalLimit::new(format!(
                "context traversal exceeds node limit {}",
                self.limits.max_nodes
            )));
        }
        Ok(())
    }

    fn visit_collection(&mut self) -> Result<(), TraversalLimit> {
        self.collection_visits = self.collection_visits.saturating_add(1);
        if self.collection_visits > self.limits.max_collection_visits {
            return Err(TraversalLimit::new(format!(
                "context traversal exceeds collection-visit limit {}",
                self.limits.max_collection_visits
            )));
        }
        Ok(())
    }

    fn visit_string(&mut self, value: &str) -> Result<(), TraversalLimit> {
        if value.len() > self.limits.max_string_bytes {
            return Err(TraversalLimit::new(format!(
                "context string is {} bytes; limit is {}",
                value.len(),
                self.limits.max_string_bytes
            )));
        }
        self.string_bytes = self.string_bytes.checked_add(value.len()).ok_or_else(|| {
            TraversalLimit::new("context cumulative string-byte counter overflowed")
        })?;
        if self.string_bytes > self.limits.max_string_bytes {
            return Err(TraversalLimit::new(format!(
                "context traversal exceeds cumulative string-byte limit {}",
                self.limits.max_string_bytes
            )));
        }
        Ok(())
    }

    fn visit_reference(&mut self, reference: &FieldRef) -> Result<(), TraversalLimit> {
        match &reference.scope {
            ContextScope::Scenario => {}
            ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                self.visit_string(step_id)?;
            }
        }
        for segment in &reference.segments {
            self.visit_collection()?;
            if let ContextPathSegment::Field { name } = segment {
                self.visit_string(name)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ProjectionTarget<'a> {
    Type(&'a ContextType),
    Object(&'a ObjectSchema),
}

struct ResolvedProjection<'a> {
    target: ProjectionTarget<'a>,
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
}

fn resolve_context_schema_bounded(
    store: &ContextStore,
    reference: &FieldRef,
    limits: ExpressionLimits,
) -> Result<ExpressionType, SchemaResolutionError> {
    let mut budget = TraversalBudget::new(limits);
    budget
        .visit_reference(reference)
        .map_err(TraversalLimit::schema_error)?;
    let resolved = resolve_projection_target(store, reference, &mut budget)?;
    project_target(
        resolved.target,
        !resolved.required,
        resolved.nullable,
        resolved.sensitivity,
        1,
        reference,
        &mut budget,
    )
}

fn resolve_projection_target<'a>(
    store: &'a ContextStore,
    reference: &FieldRef,
    budget: &mut TraversalBudget,
) -> Result<ResolvedProjection<'a>, SchemaResolutionError> {
    if store.version == 0 || store.version > CONTEXT_SCHEMA_VERSION {
        return Err(SchemaResolutionError::new(format!(
            "context version {} is not supported (supported: 1..={CONTEXT_SCHEMA_VERSION})",
            store.version
        )));
    }
    let context = find_context_bounded(store, &reference.scope, budget)
        .map_err(TraversalLimit::schema_error)?
        .ok_or_else(|| {
            SchemaResolutionError::new(format!(
                "context schema does not contain reference {reference:?}"
            ))
        })?;
    if context.version == 0 || context.version > CONTEXT_SCHEMA_VERSION {
        return Err(SchemaResolutionError::new(format!(
            "context version {} is not supported (supported: 1..={CONTEXT_SCHEMA_VERSION})",
            context.version
        )));
    }
    if context.schema.is_some() && context.root_type.is_some() {
        return Err(SchemaResolutionError::new(format!(
            "context schema for {reference:?} has conflicting root declarations"
        )));
    }

    let mut target = if let Some(value_type) = &context.root_type {
        ProjectionTarget::Type(value_type)
    } else if let Some(schema) = &context.schema {
        ProjectionTarget::Object(schema)
    } else {
        return Err(SchemaResolutionError::new(format!(
            "context schema does not contain reference {reference:?}"
        )));
    };
    let mut required = true;
    let mut nullable = false;
    let mut sensitivity = context.sensitivity;
    for segment in &reference.segments {
        match (target, segment) {
            (ProjectionTarget::Object(schema), ContextPathSegment::Field { name }) => {
                if let Some(field) = schema.fields.get(name) {
                    required &= field.required;
                    nullable |= field.nullable;
                    sensitivity = sensitivity.combine(field.sensitivity);
                    target = ProjectionTarget::Type(&field.value_type);
                } else if let Some(value_type) = schema.additional_fields.value_type() {
                    required = false;
                    target = ProjectionTarget::Type(value_type);
                } else {
                    return Err(SchemaResolutionError::new(format!(
                        "context schema does not contain reference {reference:?}"
                    )));
                }
            }
            (
                ProjectionTarget::Type(ContextType::Object { schema }),
                ContextPathSegment::Field { name },
            ) => {
                if let Some(field) = schema.fields.get(name) {
                    required &= field.required;
                    nullable |= field.nullable;
                    sensitivity = sensitivity.combine(field.sensitivity);
                    target = ProjectionTarget::Type(&field.value_type);
                } else if let Some(value_type) = schema.additional_fields.value_type() {
                    required = false;
                    target = ProjectionTarget::Type(value_type);
                } else {
                    return Err(SchemaResolutionError::new(format!(
                        "context schema does not contain reference {reference:?}"
                    )));
                }
            }
            (
                ProjectionTarget::Type(ContextType::Array { items }),
                ContextPathSegment::Index { .. },
            ) => {
                required = false;
                target = ProjectionTarget::Type(items);
            }
            _ => {
                return Err(SchemaResolutionError::new(format!(
                    "context schema does not contain reference {reference:?}"
                )));
            }
        }
    }
    Ok(ResolvedProjection {
        target,
        required,
        nullable,
        sensitivity,
    })
}

fn find_context_bounded<'a>(
    store: &'a ContextStore,
    scope: &ContextScope,
    budget: &mut TraversalBudget,
) -> Result<Option<&'a ContextValue>, TraversalLimit> {
    for entry in store.entries() {
        budget.visit_collection()?;
        if &entry.scope == scope {
            return Ok(Some(&entry.context));
        }
    }
    Ok(None)
}

fn project_target(
    target: ProjectionTarget<'_>,
    optional: bool,
    nullable: bool,
    sensitivity: Sensitivity,
    depth: usize,
    reference: &FieldRef,
    budget: &mut TraversalBudget,
) -> Result<ExpressionType, SchemaResolutionError> {
    match target {
        ProjectionTarget::Type(value_type) => project_context_type(
            value_type,
            optional,
            nullable,
            sensitivity,
            depth,
            reference,
            budget,
        ),
        ProjectionTarget::Object(schema) => {
            budget
                .visit_node(depth)
                .map_err(TraversalLimit::schema_error)?;
            reject_secret_sensitivity(sensitivity, reference)?;
            let kind = project_object_fields(schema, sensitivity, depth, reference, budget)?;
            Ok(ExpressionType {
                kind,
                optional,
                nullable,
            })
        }
    }
}

fn project_context_type(
    value_type: &ContextType,
    optional: bool,
    nullable: bool,
    sensitivity: Sensitivity,
    depth: usize,
    reference: &FieldRef,
    budget: &mut TraversalBudget,
) -> Result<ExpressionType, SchemaResolutionError> {
    budget
        .visit_node(depth)
        .map_err(TraversalLimit::schema_error)?;
    reject_secret_sensitivity(sensitivity, reference)?;
    let kind = match value_type {
        ContextType::Any => ExpressionTypeKind::Any,
        ContextType::Null => ExpressionTypeKind::Null,
        ContextType::Boolean => ExpressionTypeKind::Bool,
        ContextType::Integer => ExpressionTypeKind::Integer,
        ContextType::Number => ExpressionTypeKind::Number,
        ContextType::String { .. } => ExpressionTypeKind::String,
        ContextType::Array { items } => ExpressionTypeKind::List(Box::new(project_context_type(
            items,
            false,
            false,
            sensitivity,
            depth + 1,
            reference,
            budget,
        )?)),
        ContextType::Object { schema } => {
            project_object_fields(schema, sensitivity, depth, reference, budget)?
        }
    };
    Ok(ExpressionType {
        kind,
        optional,
        nullable,
    })
}

fn project_object_fields(
    schema: &ObjectSchema,
    inherited: Sensitivity,
    depth: usize,
    reference: &FieldRef,
    budget: &mut TraversalBudget,
) -> Result<ExpressionTypeKind, SchemaResolutionError> {
    let mut projected = BTreeMap::new();
    for (name, field) in &schema.fields {
        budget
            .visit_collection()
            .map_err(TraversalLimit::schema_error)?;
        budget
            .visit_string(name)
            .map_err(TraversalLimit::schema_error)?;
        let field_type = project_context_type(
            &field.value_type,
            !field.required,
            field.nullable,
            inherited.combine(field.sensitivity),
            depth + 1,
            reference,
            budget,
        )?;
        // Clone only after all relevant limits have been charged.
        projected.insert(name.clone(), field_type);
    }
    if let Some(additional) = schema.additional_fields.value_type() {
        budget
            .visit_collection()
            .map_err(TraversalLimit::schema_error)?;
        // Open-object values are validated as untyped at runtime, but their
        // declared subtree must still participate in limits and secret checks.
        project_context_type(
            additional,
            true,
            false,
            inherited,
            depth + 1,
            reference,
            budget,
        )?;
    }
    Ok(ExpressionTypeKind::Object(projected))
}

fn reject_secret_sensitivity(
    sensitivity: Sensitivity,
    reference: &FieldRef,
) -> Result<(), SchemaResolutionError> {
    if sensitivity.is_secret() {
        Err(SchemaResolutionError::new(format!(
            "secret context field or subtree {reference:?} cannot be used in an expression"
        )))
    } else {
        Ok(())
    }
}

fn resolve_context_value_bounded(
    store: &ContextStore,
    reference: &FieldRef,
    limits: ExpressionLimits,
) -> EvaluationValue {
    let mut budget = TraversalBudget::new(limits);
    if let Err(error) = budget.visit_reference(reference) {
        return EvaluationValue::Error(error.evaluation_error());
    }
    let context = match find_context_bounded(store, &reference.scope, &mut budget) {
        Ok(Some(context)) => context,
        Ok(None) => {
            return EvaluationValue::unknown(format!(
                "context scope for {reference:?} is not available"
            ));
        }
        Err(error) => return EvaluationValue::Error(error.evaluation_error()),
    };
    if let Err(error) = resolve_context_schema_bounded(store, reference, limits) {
        let kind = if error.is_limit_exceeded() {
            EvaluationErrorKind::LimitExceeded
        } else {
            EvaluationErrorKind::ContractViolation
        };
        return EvaluationValue::error(kind, error.diagnostic_message());
    }

    let mut value = &context.value;
    for segment in &reference.segments {
        let next = match segment {
            ContextPathSegment::Field { name } => value.get(name),
            ContextPathSegment::Index { index } => value.get(*index),
        };
        let Some(next) = next else {
            return EvaluationValue::missing(format!("context field {reference:?} is missing"));
        };
        value = next;
    }
    match expression_value_from_json_with_budget(value, 1, &mut budget) {
        Ok(value) => EvaluationValue::Value(value),
        Err(error) => EvaluationValue::Error(error),
    }
}

fn expression_value_from_json_bounded(
    value: &serde_json::Value,
    limits: ExpressionLimits,
) -> Result<ExpressionValue, EvaluationError> {
    expression_value_from_json_with_budget(value, 1, &mut TraversalBudget::new(limits))
}

fn expression_value_from_json_with_budget(
    value: &serde_json::Value,
    depth: usize,
    budget: &mut TraversalBudget,
) -> Result<ExpressionValue, EvaluationError> {
    budget
        .visit_node(depth)
        .map_err(TraversalLimit::evaluation_error)?;
    match value {
        serde_json::Value::Null => Ok(ExpressionValue::Null),
        serde_json::Value::Bool(value) => Ok(ExpressionValue::Bool(*value)),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(ExpressionValue::Int(value))
            } else if let Some(value) = value.as_u64() {
                Ok(ExpressionValue::UInt(value))
            } else if let Some(value) = value.as_f64().filter(|value| value.is_finite()) {
                Ok(ExpressionValue::Float(value))
            } else {
                Err(EvaluationError::new(
                    EvaluationErrorKind::ContractViolation,
                    "context contains a non-finite or unsupported JSON number",
                ))
            }
        }
        serde_json::Value::String(value) => {
            budget
                .visit_string(value)
                .map_err(TraversalLimit::evaluation_error)?;
            Ok(ExpressionValue::String(value.clone()))
        }
        serde_json::Value::Array(values) => {
            let mut converted = Vec::new();
            for value in values {
                budget
                    .visit_collection()
                    .map_err(TraversalLimit::evaluation_error)?;
                converted.push(expression_value_from_json_with_budget(
                    value,
                    depth + 1,
                    budget,
                )?);
            }
            Ok(ExpressionValue::List(converted))
        }
        serde_json::Value::Object(values) => {
            let mut converted = BTreeMap::new();
            for (name, value) in values {
                budget
                    .visit_collection()
                    .map_err(TraversalLimit::evaluation_error)?;
                budget
                    .visit_string(name)
                    .map_err(TraversalLimit::evaluation_error)?;
                let value = expression_value_from_json_with_budget(value, depth + 1, budget)?;
                converted.insert(name.clone(), value);
            }
            Ok(ExpressionValue::Object(converted))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionDiagnostic {
    pub code: ExpressionDiagnosticCode,
    pub location: String,
    pub message: String,
}

impl ExpressionDiagnostic {
    fn new(
        code: ExpressionDiagnosticCode,
        location: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            location: location.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionDiagnosticCode {
    UnknownReference,
    UnknownLocal,
    UnknownField,
    TypeMismatch,
    ExpectedBoolean,
    InvalidLiteral,
    InvalidRegex,
    InvalidBinding,
    LimitExceeded,
}

/// Immutable checked program. Its fields are private so an unchecked AST
/// cannot be smuggled into the evaluator.
#[derive(Debug, Clone)]
pub struct CheckedExpressionV1 {
    expression: ExpressionV1,
    result_type: ExpressionType,
    limits: ExpressionLimits,
    regexes: BTreeMap<String, Regex>,
    resolved_references: Vec<(FieldRef, ExpressionType)>,
}

impl CheckedExpressionV1 {
    pub fn result_type(&self) -> &ExpressionType {
        &self.result_type
    }

    pub fn expression(&self) -> &ExpressionV1 {
        &self.expression
    }

    pub fn limits(&self) -> ExpressionLimits {
        self.limits
    }
}

/// Type-check a value expression and compile all regular expressions.
pub fn check_value_expression(
    expression: ExpressionV1,
    resolver: &dyn ExpressionSchemaResolver,
    limits: ExpressionLimits,
) -> Result<CheckedExpressionV1, Vec<ExpressionDiagnostic>> {
    let mut checker = Checker::new(resolver, limits);
    let result_type = checker.infer_expression(&expression, "$", 1);
    checker.finish(expression, result_type)
}

/// Type-check a Boolean rule. A nullable or optional Boolean reference is a
/// valid rule type, but it can evaluate to `Null`/`Missing` at runtime; callers
/// must apply an explicit execution policy to that state.
pub fn check_rule(
    expression: RuleExprV1,
    resolver: &dyn ExpressionSchemaResolver,
    limits: ExpressionLimits,
) -> Result<CheckedExpressionV1, Vec<ExpressionDiagnostic>> {
    let mut checker = Checker::new(resolver, limits);
    let result_type = checker.infer_expression(&expression, "$", 1);
    if let Some(result_type) = &result_type {
        if !matches!(&result_type.kind, ExpressionTypeKind::Bool) {
            checker.diagnostics.push(ExpressionDiagnostic::new(
                ExpressionDiagnosticCode::ExpectedBoolean,
                "$",
                format!(
                    "rule root must have bool type, found {}",
                    result_type.describe()
                ),
            ));
        }
    }
    checker.finish(expression, result_type)
}

struct Checker<'a> {
    resolver: &'a dyn ExpressionSchemaResolver,
    limits: ExpressionLimits,
    diagnostics: Vec<ExpressionDiagnostic>,
    nodes: usize,
    string_bytes: usize,
    quantifier_depth: usize,
    locals: Vec<(String, ExpressionType)>,
    regexes: BTreeMap<String, Regex>,
    resolved_references: Vec<(FieldRef, ExpressionType)>,
}

impl<'a> Checker<'a> {
    fn new(resolver: &'a dyn ExpressionSchemaResolver, limits: ExpressionLimits) -> Self {
        Self {
            resolver,
            limits,
            diagnostics: Vec::new(),
            nodes: 0,
            string_bytes: 0,
            quantifier_depth: 0,
            locals: Vec::new(),
            regexes: BTreeMap::new(),
            resolved_references: Vec::new(),
        }
    }

    fn finish(
        self,
        expression: ExpressionV1,
        result_type: Option<ExpressionType>,
    ) -> Result<CheckedExpressionV1, Vec<ExpressionDiagnostic>> {
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        let Some(result_type) = result_type else {
            return Err(vec![ExpressionDiagnostic::new(
                ExpressionDiagnosticCode::TypeMismatch,
                "$",
                "expression type could not be inferred",
            )]);
        };
        Ok(CheckedExpressionV1 {
            expression,
            result_type,
            limits: self.limits,
            regexes: self.regexes,
            resolved_references: self.resolved_references,
        })
    }

    fn infer_expression(
        &mut self,
        expression: &ExpressionV1,
        location: &str,
        depth: usize,
    ) -> Option<ExpressionType> {
        if !self.visit_node(location, depth) {
            return None;
        }
        match expression {
            ExpressionV1::Literal { value } => self.infer_literal(value, location, depth),
            ExpressionV1::Ref { reference } => self.resolve_reference(reference, location),
            ExpressionV1::All { expressions } | ExpressionV1::Any { expressions } => {
                for (index, child) in expressions.iter().enumerate() {
                    let child_location = format!("{location}.expressions[{index}]");
                    if let Some(child_type) =
                        self.infer_expression(child, &child_location, depth + 1)
                    {
                        self.require_bool(&child_type, &child_location);
                    }
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Not { expression } => {
                let child_location = format!("{location}.expression");
                if let Some(child_type) =
                    self.infer_expression(expression, &child_location, depth + 1)
                {
                    self.require_bool(&child_type, &child_location);
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Exists { reference } => {
                self.resolve_reference(reference, &format!("{location}.reference"));
                Some(ExpressionType::bool())
            }
            ExpressionV1::IsNull { expression } => {
                self.infer_expression(expression, &format!("{location}.expression"), depth + 1);
                Some(ExpressionType::bool())
            }
            ExpressionV1::IsEmpty { expression } => {
                let child_location = format!("{location}.expression");
                if let Some(child_type) =
                    self.infer_expression(expression, &child_location, depth + 1)
                {
                    let supported = matches!(
                        &child_type.kind,
                        ExpressionTypeKind::String
                            | ExpressionTypeKind::List(_)
                            | ExpressionTypeKind::Object(_)
                            | ExpressionTypeKind::Any
                    );
                    if !supported {
                        self.type_mismatch(&child_location, "string, list, or object", &child_type);
                    }
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Compare {
                operator,
                left,
                right,
            } => {
                let left_location = format!("{location}.left");
                let right_location = format!("{location}.right");
                let left_type = self.infer_expression(left, &left_location, depth + 1);
                let right_type = self.infer_expression(right, &right_location, depth + 1);
                if let (Some(left_type), Some(right_type)) = (&left_type, &right_type) {
                    let compatible = match operator {
                        ComparisonOperator::Equal | ComparisonOperator::NotEqual => {
                            equality_types_compatible(left_type, right_type)
                        }
                        _ => ordering_types_compatible(left_type, right_type),
                    };
                    if !compatible {
                        self.diagnostics.push(ExpressionDiagnostic::new(
                            ExpressionDiagnosticCode::TypeMismatch,
                            location,
                            format!(
                                "operator {operator:?} cannot compare {} with {}",
                                left_type.describe(),
                                right_type.describe()
                            ),
                        ));
                    }
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Contains { value, needle }
            | ExpressionV1::StartsWith {
                value,
                prefix: needle,
            }
            | ExpressionV1::EndsWith {
                value,
                suffix: needle,
            } => {
                let value_location = format!("{location}.value");
                let needle_location = format!("{location}.operand");
                if let Some(value_type) = self.infer_expression(value, &value_location, depth + 1) {
                    self.require_string(&value_type, &value_location);
                }
                if let Some(needle_type) =
                    self.infer_expression(needle, &needle_location, depth + 1)
                {
                    self.require_string(&needle_type, &needle_location);
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Matches { value, pattern } => {
                let value_location = format!("{location}.value");
                if let Some(value_type) = self.infer_expression(value, &value_location, depth + 1) {
                    self.require_string(&value_type, &value_location);
                }
                self.compile_regex(pattern, &format!("{location}.pattern"));
                Some(ExpressionType::bool())
            }
            ExpressionV1::In { needle, collection } => {
                let needle_location = format!("{location}.needle");
                let collection_location = format!("{location}.collection");
                let needle_type = self.infer_expression(needle, &needle_location, depth + 1);
                let collection_type =
                    self.infer_expression(collection, &collection_location, depth + 1);
                if let Some(collection_type) = &collection_type {
                    match &collection_type.kind {
                        ExpressionTypeKind::List(item_type) => {
                            if let Some(needle_type) = &needle_type {
                                if !equality_types_compatible(needle_type, item_type) {
                                    self.diagnostics.push(ExpressionDiagnostic::new(
                                        ExpressionDiagnosticCode::TypeMismatch,
                                        location,
                                        format!(
                                            "in operand {} is incompatible with collection item {}",
                                            needle_type.describe(),
                                            item_type.describe()
                                        ),
                                    ));
                                }
                            }
                        }
                        ExpressionTypeKind::Any => {}
                        _ => self.type_mismatch(&collection_location, "list", collection_type),
                    }
                }
                Some(ExpressionType::bool())
            }
            ExpressionV1::Quantifier {
                quantifier: _,
                collection,
                binding,
                predicate,
            } => self.infer_quantifier(collection, binding, predicate, location, depth),
        }
    }

    fn infer_quantifier(
        &mut self,
        collection: &ExpressionV1,
        binding: &str,
        predicate: &ExpressionV1,
        location: &str,
        depth: usize,
    ) -> Option<ExpressionType> {
        let collection_location = format!("{location}.collection");
        let collection_type = self.infer_expression(collection, &collection_location, depth + 1);
        let item_type = match collection_type.as_ref().map(|value| &value.kind) {
            Some(ExpressionTypeKind::List(item)) => Some((**item).clone()),
            Some(ExpressionTypeKind::Any) => {
                Some(ExpressionType::required(ExpressionTypeKind::Any))
            }
            Some(_) => {
                if let Some(collection_type) = &collection_type {
                    self.type_mismatch(&collection_location, "list", collection_type);
                }
                None
            }
            None => None,
        };

        let binding_location = format!("{location}.binding");
        let valid_binding = !binding.is_empty()
            && binding.len() <= self.limits.max_string_bytes
            && !self.locals.iter().any(|(name, _)| name == binding);
        if !valid_binding {
            self.diagnostics.push(ExpressionDiagnostic::new(
                ExpressionDiagnosticCode::InvalidBinding,
                &binding_location,
                "quantifier binding must be non-empty, bounded, and must not shadow an active binding",
            ));
        }

        self.quantifier_depth += 1;
        if self.quantifier_depth > self.limits.max_quantifier_depth {
            self.push_limit_once(
                &binding_location,
                format!(
                    "quantifier nesting exceeds limit {}",
                    self.limits.max_quantifier_depth
                ),
            );
        }
        let pushed_binding = if valid_binding {
            if let Some(item_type) = item_type {
                self.locals.push((binding.to_owned(), item_type));
                true
            } else {
                false
            }
        } else {
            false
        };
        let predicate_location = format!("{location}.predicate");
        if let Some(predicate_type) =
            self.infer_expression(predicate, &predicate_location, depth + 1)
        {
            self.require_bool(&predicate_type, &predicate_location);
        }
        if pushed_binding {
            self.locals.pop();
        }
        self.quantifier_depth -= 1;
        Some(ExpressionType::bool())
    }

    fn resolve_reference(
        &mut self,
        reference: &ReferenceV1,
        location: &str,
    ) -> Option<ExpressionType> {
        match reference {
            ReferenceV1::Context { field } => {
                if let Some((_, value_type)) = self
                    .resolved_references
                    .iter()
                    .find(|(existing, _)| existing == field)
                {
                    return Some(value_type.clone());
                }
                match self.resolver.resolve_schema_bounded(field, self.limits) {
                    Ok(value_type) => {
                        self.resolved_references
                            .push((field.clone(), value_type.clone()));
                        Some(value_type)
                    }
                    Err(error) => {
                        let code = if error.is_limit_exceeded() {
                            ExpressionDiagnosticCode::LimitExceeded
                        } else {
                            ExpressionDiagnosticCode::UnknownReference
                        };
                        self.diagnostics.push(ExpressionDiagnostic::new(
                            code,
                            location,
                            error.diagnostic_message(),
                        ));
                        None
                    }
                }
            }
            ReferenceV1::Local { binding, path } => {
                let Some((_, root_type)) = self
                    .locals
                    .iter()
                    .rev()
                    .find(|(candidate, _)| candidate == binding)
                else {
                    self.diagnostics.push(ExpressionDiagnostic::new(
                        ExpressionDiagnosticCode::UnknownLocal,
                        location,
                        format!("local binding {binding:?} is not in scope"),
                    ));
                    return None;
                };
                let mut current = root_type.clone();
                for (index, segment) in path.iter().enumerate() {
                    if segment.len() > self.limits.max_string_bytes {
                        self.push_limit_once(
                            location,
                            format!(
                                "local path segment exceeds string limit {}",
                                self.limits.max_string_bytes
                            ),
                        );
                        return None;
                    }
                    match &current.kind {
                        ExpressionTypeKind::Object(fields) => {
                            let Some(child) = fields.get(segment) else {
                                self.diagnostics.push(ExpressionDiagnostic::new(
                                    ExpressionDiagnosticCode::UnknownField,
                                    location,
                                    format!(
                                        "local binding {binding:?} has no field {segment:?} at path segment {index}"
                                    ),
                                ));
                                return None;
                            };
                            let inherited_optional = current.optional;
                            let inherited_nullable = current.nullable;
                            current = child.clone();
                            current.optional |= inherited_optional;
                            current.nullable |= inherited_nullable;
                        }
                        ExpressionTypeKind::Any => {
                            current = ExpressionType {
                                kind: ExpressionTypeKind::Any,
                                optional: true,
                                nullable: true,
                            };
                        }
                        _ => {
                            self.diagnostics.push(ExpressionDiagnostic::new(
                                ExpressionDiagnosticCode::TypeMismatch,
                                location,
                                format!(
                                    "cannot select field {segment:?} from {}",
                                    current.describe()
                                ),
                            ));
                            return None;
                        }
                    }
                }
                Some(current)
            }
        }
    }

    fn infer_literal(
        &mut self,
        value: &ExpressionValue,
        location: &str,
        depth: usize,
    ) -> Option<ExpressionType> {
        match value {
            ExpressionValue::Null => Some(ExpressionType::null()),
            ExpressionValue::Bool(_) => Some(ExpressionType::bool()),
            ExpressionValue::Int(_) | ExpressionValue::UInt(_) => {
                Some(ExpressionType::required(ExpressionTypeKind::Integer))
            }
            ExpressionValue::Float(value) => {
                if !value.is_finite() {
                    self.diagnostics.push(ExpressionDiagnostic::new(
                        ExpressionDiagnosticCode::InvalidLiteral,
                        location,
                        "floating-point literals must be finite",
                    ));
                    None
                } else {
                    Some(ExpressionType::required(ExpressionTypeKind::Number))
                }
            }
            ExpressionValue::String(value) => {
                self.check_string(value, location);
                Some(ExpressionType::required(ExpressionTypeKind::String))
            }
            ExpressionValue::List(values) => {
                if values.is_empty() {
                    return Some(ExpressionType::list(ExpressionType::required(
                        ExpressionTypeKind::Never,
                    )));
                }
                let mut item_type: Option<ExpressionType> = None;
                for (index, value) in values.iter().enumerate() {
                    let child_location = format!("{location}.value[{index}]");
                    if !self.visit_node(&child_location, depth + 1) {
                        continue;
                    }
                    let Some(next_type) = self.infer_literal(value, &child_location, depth + 1)
                    else {
                        continue;
                    };
                    item_type = match item_type {
                        None => Some(next_type),
                        Some(current) => match common_expression_type(&current, &next_type) {
                            Some(common) => Some(common),
                            None => {
                                self.diagnostics.push(ExpressionDiagnostic::new(
                                    ExpressionDiagnosticCode::TypeMismatch,
                                    &child_location,
                                    format!(
                                        "list literal mixes incompatible item types {} and {}",
                                        current.describe(),
                                        next_type.describe()
                                    ),
                                ));
                                Some(current)
                            }
                        },
                    };
                }
                item_type.map(ExpressionType::list)
            }
            ExpressionValue::Object(values) => {
                let mut fields = BTreeMap::new();
                for (name, value) in values {
                    let child_location = format!("{location}.value.{name}");
                    self.check_string(name, &child_location);
                    if !self.visit_node(&child_location, depth + 1) {
                        continue;
                    }
                    if let Some(value_type) = self.infer_literal(value, &child_location, depth + 1)
                    {
                        fields.insert(name.clone(), value_type);
                    }
                }
                Some(ExpressionType::object(fields))
            }
        }
    }

    fn visit_node(&mut self, location: &str, depth: usize) -> bool {
        self.nodes = self.nodes.saturating_add(1);
        let mut allowed = true;
        if self.nodes > self.limits.max_nodes {
            self.push_limit_once(
                location,
                format!("expression exceeds node limit {}", self.limits.max_nodes),
            );
            allowed = false;
        }
        if depth > self.limits.max_depth {
            self.push_limit_once(
                location,
                format!("expression exceeds depth limit {}", self.limits.max_depth),
            );
            allowed = false;
        }
        allowed
    }

    fn check_string(&mut self, value: &str, location: &str) {
        if value.len() > self.limits.max_string_bytes {
            self.push_limit_once(
                location,
                format!(
                    "string is {} bytes; limit is {}",
                    value.len(),
                    self.limits.max_string_bytes
                ),
            );
            return;
        }
        self.string_bytes = self.string_bytes.saturating_add(value.len());
        if self.string_bytes > self.limits.max_string_bytes {
            self.push_limit_once(
                location,
                format!(
                    "expression exceeds cumulative string-byte limit {}",
                    self.limits.max_string_bytes
                ),
            );
        }
    }

    fn compile_regex(&mut self, pattern: &str, location: &str) {
        self.check_string(pattern, location);
        if pattern.len() > self.limits.max_regex_pattern_bytes {
            self.push_limit_once(
                location,
                format!(
                    "regex pattern is {} bytes; limit is {}",
                    pattern.len(),
                    self.limits.max_regex_pattern_bytes
                ),
            );
            return;
        }
        if self.regexes.contains_key(pattern) {
            return;
        }
        match RegexBuilder::new(pattern)
            .size_limit(self.limits.max_regex_compiled_bytes)
            .dfa_size_limit(self.limits.max_regex_compiled_bytes)
            .build()
        {
            Ok(regex) => {
                self.regexes.insert(pattern.to_owned(), regex);
            }
            Err(error) => self.diagnostics.push(ExpressionDiagnostic::new(
                ExpressionDiagnosticCode::InvalidRegex,
                location,
                error.to_string(),
            )),
        }
    }

    fn require_bool(&mut self, value_type: &ExpressionType, location: &str) {
        if !matches!(
            &value_type.kind,
            ExpressionTypeKind::Bool | ExpressionTypeKind::Any
        ) {
            self.type_mismatch(location, "bool", value_type);
        }
    }

    fn require_string(&mut self, value_type: &ExpressionType, location: &str) {
        if !matches!(
            &value_type.kind,
            ExpressionTypeKind::String | ExpressionTypeKind::Any
        ) {
            self.type_mismatch(location, "string", value_type);
        }
    }

    fn type_mismatch(&mut self, location: &str, expected: &str, actual: &ExpressionType) {
        self.diagnostics.push(ExpressionDiagnostic::new(
            ExpressionDiagnosticCode::TypeMismatch,
            location,
            format!("expected {expected}, found {}", actual.describe()),
        ));
    }

    fn push_limit_once(&mut self, location: &str, message: String) {
        if !self.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == ExpressionDiagnosticCode::LimitExceeded
                && diagnostic.message == message
        }) {
            self.diagnostics.push(ExpressionDiagnostic::new(
                ExpressionDiagnosticCode::LimitExceeded,
                location,
                message,
            ));
        }
    }
}

fn common_expression_type(left: &ExpressionType, right: &ExpressionType) -> Option<ExpressionType> {
    if left.kind == ExpressionTypeKind::Never {
        return Some(right.clone());
    }
    if right.kind == ExpressionTypeKind::Never {
        return Some(left.clone());
    }
    if left.kind == ExpressionTypeKind::Null {
        let mut common = right.clone();
        common.nullable = true;
        return Some(common);
    }
    if right.kind == ExpressionTypeKind::Null {
        let mut common = left.clone();
        common.nullable = true;
        return Some(common);
    }
    let kind = match (&left.kind, &right.kind) {
        (ExpressionTypeKind::Any, _) | (_, ExpressionTypeKind::Any) => ExpressionTypeKind::Any,
        (ExpressionTypeKind::Integer, ExpressionTypeKind::Number)
        | (ExpressionTypeKind::Number, ExpressionTypeKind::Integer) => ExpressionTypeKind::Number,
        (left, right) if left == right => left.clone(),
        _ => return None,
    };
    Some(ExpressionType {
        kind,
        optional: left.optional || right.optional,
        nullable: left.nullable || right.nullable,
    })
}

fn equality_types_compatible(left: &ExpressionType, right: &ExpressionType) -> bool {
    if matches!(
        &left.kind,
        ExpressionTypeKind::Any | ExpressionTypeKind::Never
    ) || matches!(
        &right.kind,
        ExpressionTypeKind::Any | ExpressionTypeKind::Never
    ) {
        return true;
    }
    if left.kind == ExpressionTypeKind::Null {
        return right.kind == ExpressionTypeKind::Null || right.nullable;
    }
    if right.kind == ExpressionTypeKind::Null {
        return left.kind == ExpressionTypeKind::Null || left.nullable;
    }
    common_expression_type(left, right).is_some()
}

fn ordering_types_compatible(left: &ExpressionType, right: &ExpressionType) -> bool {
    if matches!(&left.kind, ExpressionTypeKind::Any)
        || matches!(&right.kind, ExpressionTypeKind::Any)
    {
        return true;
    }
    matches!(
        (&left.kind, &right.kind),
        (ExpressionTypeKind::Integer, ExpressionTypeKind::Integer)
            | (ExpressionTypeKind::Integer, ExpressionTypeKind::Number)
            | (ExpressionTypeKind::Number, ExpressionTypeKind::Integer)
            | (ExpressionTypeKind::Number, ExpressionTypeKind::Number)
            | (ExpressionTypeKind::String, ExpressionTypeKind::String)
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionEvaluation {
    pub outcome: EvaluationValue,
    pub operations: usize,
    pub collection_visits: usize,
}

impl CheckedExpressionV1 {
    /// Evaluate using only the supplied immutable activation.
    pub fn evaluate(&self, resolver: &dyn ExpressionValueResolver) -> EvaluationValue {
        self.evaluate_with_stats(resolver).outcome
    }

    /// Evaluate this checked program as a Boolean rule without collapsing
    /// missing, null, unknown, or error states.
    pub fn evaluate_rule(&self, resolver: &dyn ExpressionValueResolver) -> RuleEvaluation {
        match self.evaluate(resolver) {
            EvaluationValue::Value(ExpressionValue::Bool(true)) => RuleEvaluation::True,
            EvaluationValue::Value(ExpressionValue::Bool(false)) => RuleEvaluation::False,
            EvaluationValue::Value(ExpressionValue::Null) => RuleEvaluation::Null,
            EvaluationValue::Value(value) => RuleEvaluation::Error(EvaluationError::new(
                EvaluationErrorKind::InvalidOperand,
                format!(
                    "checked rule produced {} instead of bool",
                    runtime_type_name(&value)
                ),
            )),
            EvaluationValue::Missing(issue) => RuleEvaluation::Missing(issue),
            EvaluationValue::Unknown(issue) => RuleEvaluation::Unknown(issue),
            EvaluationValue::Error(error) => RuleEvaluation::Error(error),
        }
    }

    pub fn evaluate_with_stats(
        &self,
        resolver: &dyn ExpressionValueResolver,
    ) -> ExpressionEvaluation {
        let mut evaluator = Evaluator::new(self, resolver);
        let outcome = evaluator.evaluate(&self.expression);
        ExpressionEvaluation {
            outcome,
            operations: evaluator.operations,
            collection_visits: evaluator.collection_visits,
        }
    }
}

struct Evaluator<'a> {
    program: &'a CheckedExpressionV1,
    resolver: &'a dyn ExpressionValueResolver,
    operations: usize,
    collection_visits: usize,
    string_bytes: usize,
    locals: Vec<(String, ExpressionValue)>,
    resolved_values: BTreeMap<FieldRef, EvaluationValue>,
}

impl<'a> Evaluator<'a> {
    fn new(program: &'a CheckedExpressionV1, resolver: &'a dyn ExpressionValueResolver) -> Self {
        Self {
            program,
            resolver,
            operations: 0,
            collection_visits: 0,
            string_bytes: 0,
            locals: Vec::new(),
            resolved_values: BTreeMap::new(),
        }
    }

    fn evaluate(&mut self, expression: &ExpressionV1) -> EvaluationValue {
        if let Err(error) = self.consume_operation() {
            return EvaluationValue::Error(error);
        }
        match expression {
            ExpressionV1::Literal { value } => {
                if let Err(error) = self.check_value_shallow(value) {
                    EvaluationValue::Error(error)
                } else {
                    EvaluationValue::Value(value.clone())
                }
            }
            ExpressionV1::Ref { reference } => self.evaluate_reference(reference),
            ExpressionV1::All { expressions } => self.evaluate_logical(expressions, false),
            ExpressionV1::Any { expressions } => self.evaluate_logical(expressions, true),
            ExpressionV1::Not { expression } => match self.evaluate_bool(expression) {
                EvaluationValue::Value(ExpressionValue::Bool(value)) => {
                    EvaluationValue::Value(ExpressionValue::Bool(!value))
                }
                other => other,
            },
            ExpressionV1::Exists { reference } => match self.evaluate_reference(reference) {
                EvaluationValue::Value(_) => EvaluationValue::Value(ExpressionValue::Bool(true)),
                EvaluationValue::Missing(_) => EvaluationValue::Value(ExpressionValue::Bool(false)),
                other => other,
            },
            ExpressionV1::IsNull { expression } => match self.evaluate(expression) {
                EvaluationValue::Value(ExpressionValue::Null) => {
                    EvaluationValue::Value(ExpressionValue::Bool(true))
                }
                EvaluationValue::Value(_) => EvaluationValue::Value(ExpressionValue::Bool(false)),
                other => other,
            },
            ExpressionV1::IsEmpty { expression } => match self.evaluate(expression) {
                EvaluationValue::Value(ExpressionValue::String(value)) => {
                    EvaluationValue::Value(ExpressionValue::Bool(value.is_empty()))
                }
                EvaluationValue::Value(ExpressionValue::List(value)) => {
                    EvaluationValue::Value(ExpressionValue::Bool(value.is_empty()))
                }
                EvaluationValue::Value(ExpressionValue::Object(value)) => {
                    EvaluationValue::Value(ExpressionValue::Bool(value.is_empty()))
                }
                EvaluationValue::Value(value) => self.invalid_operand(format!(
                    "is-empty expected string, list, or object; found {}",
                    runtime_type_name(&value)
                )),
                other => other,
            },
            ExpressionV1::Compare {
                operator,
                left,
                right,
            } => self.evaluate_comparison(*operator, left, right),
            ExpressionV1::Contains { value, needle } => {
                self.evaluate_string_predicate(value, needle, |value, needle| {
                    value.contains(needle)
                })
            }
            ExpressionV1::StartsWith { value, prefix } => {
                self.evaluate_string_predicate(value, prefix, |value, prefix| {
                    value.starts_with(prefix)
                })
            }
            ExpressionV1::EndsWith { value, suffix } => {
                self.evaluate_string_predicate(value, suffix, |value, suffix| {
                    value.ends_with(suffix)
                })
            }
            ExpressionV1::Matches { value, pattern } => self.evaluate_regex(value, pattern),
            ExpressionV1::In { needle, collection } => self.evaluate_in(needle, collection),
            ExpressionV1::Quantifier {
                quantifier,
                collection,
                binding,
                predicate,
            } => self.evaluate_quantifier(*quantifier, collection, binding, predicate),
        }
    }

    fn evaluate_reference(&mut self, reference: &ReferenceV1) -> EvaluationValue {
        match reference {
            ReferenceV1::Context { field } => {
                let outcome = if let Some(value) = self.resolved_values.get(field) {
                    value.clone()
                } else {
                    let value = self
                        .resolver
                        .resolve_value_bounded(field, self.program.limits);
                    self.resolved_values.insert(field.clone(), value.clone());
                    value
                };
                let expected = self
                    .program
                    .resolved_references
                    .iter()
                    .find(|(candidate, _)| candidate == field)
                    .map(|(_, value_type)| value_type.clone());
                let Some(expected) = expected else {
                    return EvaluationValue::error(
                        EvaluationErrorKind::Internal,
                        format!("checked program has no type for reference {field:?}"),
                    );
                };
                match outcome {
                    EvaluationValue::Value(value) => {
                        match self.validate_runtime_value(&value, &expected) {
                            Ok(()) => EvaluationValue::Value(value),
                            Err(error) => EvaluationValue::Error(error),
                        }
                    }
                    EvaluationValue::Missing(issue) if !expected.optional => {
                        EvaluationValue::error(
                            EvaluationErrorKind::ContractViolation,
                            format!(
                                "required context field {field:?} is missing: {}",
                                issue.message
                            ),
                        )
                    }
                    other => other,
                }
            }
            ReferenceV1::Local { binding, path } => {
                let Some((_, root)) = self
                    .locals
                    .iter()
                    .rev()
                    .find(|(candidate, _)| candidate == binding)
                else {
                    return EvaluationValue::error(
                        EvaluationErrorKind::Internal,
                        format!("checked local binding {binding:?} is not available"),
                    );
                };
                let mut current = root.clone();
                for segment in path {
                    if let Err(error) = self.consume_operation() {
                        return EvaluationValue::Error(error);
                    }
                    current = match current {
                        ExpressionValue::Object(mut fields) => {
                            let Some(value) = fields.remove(segment) else {
                                return EvaluationValue::missing(format!(
                                    "local binding {binding:?} has no field {segment:?}"
                                ));
                            };
                            value
                        }
                        ExpressionValue::Null => {
                            return self.invalid_operand(format!(
                                "cannot select field {segment:?} from null local binding {binding:?}"
                            ));
                        }
                        value => {
                            return self.invalid_operand(format!(
                                "cannot select field {segment:?} from {} local binding {binding:?}",
                                runtime_type_name(&value)
                            ));
                        }
                    };
                }
                if let Err(error) = self.check_value_shallow(&current) {
                    EvaluationValue::Error(error)
                } else {
                    EvaluationValue::Value(current)
                }
            }
        }
    }

    fn evaluate_logical(&mut self, expressions: &[ExpressionV1], is_any: bool) -> EvaluationValue {
        let decisive = is_any;
        let identity = !is_any;
        let mut deferred = None;
        for expression in expressions {
            match self.evaluate_bool(expression) {
                EvaluationValue::Value(ExpressionValue::Bool(value)) if value == decisive => {
                    return EvaluationValue::Value(ExpressionValue::Bool(decisive));
                }
                EvaluationValue::Value(ExpressionValue::Bool(_)) => {}
                issue => merge_deferred(&mut deferred, issue),
            }
        }
        deferred.unwrap_or(EvaluationValue::Value(ExpressionValue::Bool(identity)))
    }

    fn evaluate_bool(&mut self, expression: &ExpressionV1) -> EvaluationValue {
        match self.evaluate(expression) {
            EvaluationValue::Value(ExpressionValue::Bool(value)) => {
                EvaluationValue::Value(ExpressionValue::Bool(value))
            }
            EvaluationValue::Value(value) => self.invalid_operand(format!(
                "Boolean expression produced {}",
                runtime_type_name(&value)
            )),
            other => other,
        }
    }

    fn evaluate_comparison(
        &mut self,
        operator: ComparisonOperator,
        left: &ExpressionV1,
        right: &ExpressionV1,
    ) -> EvaluationValue {
        let left = match self.evaluate(left) {
            EvaluationValue::Value(value) => value,
            other => return other,
        };
        let right = match self.evaluate(right) {
            EvaluationValue::Value(value) => value,
            other => return other,
        };
        let result = match operator {
            ComparisonOperator::Equal | ComparisonOperator::NotEqual => {
                match self.values_equal(&left, &right) {
                    Ok(equal) => {
                        if operator == ComparisonOperator::Equal {
                            equal
                        } else {
                            !equal
                        }
                    }
                    Err(error) => return EvaluationValue::Error(error),
                }
            }
            _ => {
                let ordering = match self.values_ordering(&left, &right) {
                    Ok(ordering) => ordering,
                    Err(error) => return EvaluationValue::Error(error),
                };
                match operator {
                    ComparisonOperator::LessThan => ordering.is_lt(),
                    ComparisonOperator::LessThanOrEqual => ordering.is_le(),
                    ComparisonOperator::GreaterThan => ordering.is_gt(),
                    ComparisonOperator::GreaterThanOrEqual => ordering.is_ge(),
                    ComparisonOperator::Equal | ComparisonOperator::NotEqual => unreachable!(),
                }
            }
        };
        EvaluationValue::Value(ExpressionValue::Bool(result))
    }

    fn evaluate_string_predicate(
        &mut self,
        value: &ExpressionV1,
        operand: &ExpressionV1,
        predicate: impl FnOnce(&str, &str) -> bool,
    ) -> EvaluationValue {
        let value = match self.evaluate(value) {
            EvaluationValue::Value(ExpressionValue::String(value)) => value,
            EvaluationValue::Value(value) => {
                return self.invalid_operand(format!(
                    "string predicate expected string; found {}",
                    runtime_type_name(&value)
                ));
            }
            other => return other,
        };
        let operand = match self.evaluate(operand) {
            EvaluationValue::Value(ExpressionValue::String(value)) => value,
            EvaluationValue::Value(value) => {
                return self.invalid_operand(format!(
                    "string predicate expected string operand; found {}",
                    runtime_type_name(&value)
                ));
            }
            other => return other,
        };
        if let Err(error) = self.check_string(&value) {
            return EvaluationValue::Error(error);
        }
        if let Err(error) = self.check_string(&operand) {
            return EvaluationValue::Error(error);
        }
        EvaluationValue::Value(ExpressionValue::Bool(predicate(&value, &operand)))
    }

    fn evaluate_regex(&mut self, expression: &ExpressionV1, pattern: &str) -> EvaluationValue {
        let value = match self.evaluate(expression) {
            EvaluationValue::Value(ExpressionValue::String(value)) => value,
            EvaluationValue::Value(value) => {
                return self.invalid_operand(format!(
                    "matches expected string; found {}",
                    runtime_type_name(&value)
                ));
            }
            other => return other,
        };
        if let Err(error) = self.check_string(&value) {
            return EvaluationValue::Error(error);
        }
        if value.len() > self.program.limits.max_regex_input_bytes {
            return EvaluationValue::error(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "regex input is {} bytes; limit is {}",
                    value.len(),
                    self.program.limits.max_regex_input_bytes
                ),
            );
        }
        let Some(regex) = self.program.regexes.get(pattern) else {
            return EvaluationValue::error(
                EvaluationErrorKind::Internal,
                "checked program is missing a compiled regular expression",
            );
        };
        EvaluationValue::Value(ExpressionValue::Bool(regex.is_match(&value)))
    }

    fn evaluate_in(&mut self, needle: &ExpressionV1, collection: &ExpressionV1) -> EvaluationValue {
        let needle = match self.evaluate(needle) {
            EvaluationValue::Value(value) => value,
            other => return other,
        };
        let values = match self.evaluate(collection) {
            EvaluationValue::Value(ExpressionValue::List(values)) => values,
            EvaluationValue::Value(value) => {
                return self.invalid_operand(format!(
                    "in expected list; found {}",
                    runtime_type_name(&value)
                ));
            }
            other => return other,
        };
        for value in values {
            if let Err(error) = self.visit_collection() {
                return EvaluationValue::Error(error);
            }
            match self.values_equal(&needle, &value) {
                Ok(true) => return EvaluationValue::Value(ExpressionValue::Bool(true)),
                Ok(false) => {}
                Err(error) => return EvaluationValue::Error(error),
            }
        }
        EvaluationValue::Value(ExpressionValue::Bool(false))
    }

    fn evaluate_quantifier(
        &mut self,
        quantifier: CollectionQuantifier,
        collection: &ExpressionV1,
        binding: &str,
        predicate: &ExpressionV1,
    ) -> EvaluationValue {
        let values = match self.evaluate(collection) {
            EvaluationValue::Value(ExpressionValue::List(values)) => values,
            EvaluationValue::Value(value) => {
                return self.invalid_operand(format!(
                    "quantifier expected list; found {}",
                    runtime_type_name(&value)
                ));
            }
            other => return other,
        };
        let mut deferred = None;
        for value in values {
            if let Err(error) = self.visit_collection() {
                return EvaluationValue::Error(error);
            }
            self.locals.push((binding.to_owned(), value));
            let predicate_result = self.evaluate_bool(predicate);
            self.locals.pop();
            match (quantifier, predicate_result) {
                (_, EvaluationValue::Value(ExpressionValue::Bool(value)))
                    if quantifier == CollectionQuantifier::Any && value =>
                {
                    return EvaluationValue::Value(ExpressionValue::Bool(true));
                }
                (_, EvaluationValue::Value(ExpressionValue::Bool(value)))
                    if quantifier == CollectionQuantifier::All && !value =>
                {
                    return EvaluationValue::Value(ExpressionValue::Bool(false));
                }
                (_, EvaluationValue::Value(ExpressionValue::Bool(value)))
                    if quantifier == CollectionQuantifier::None && value =>
                {
                    return EvaluationValue::Value(ExpressionValue::Bool(false));
                }
                (_, EvaluationValue::Value(ExpressionValue::Bool(_))) => {}
                (_, issue) => merge_deferred(&mut deferred, issue),
            }
        }
        deferred.unwrap_or(EvaluationValue::Value(ExpressionValue::Bool(
            match quantifier {
                CollectionQuantifier::Any => false,
                CollectionQuantifier::All | CollectionQuantifier::None => true,
            },
        )))
    }

    fn validate_runtime_value(
        &mut self,
        value: &ExpressionValue,
        expected: &ExpressionType,
    ) -> Result<(), EvaluationError> {
        self.validate_runtime_value_at(value, expected, 1)
    }

    fn validate_runtime_value_at(
        &mut self,
        value: &ExpressionValue,
        expected: &ExpressionType,
        depth: usize,
    ) -> Result<(), EvaluationError> {
        self.check_runtime_depth(depth)?;
        if matches!(value, ExpressionValue::Null) {
            if expected.nullable
                || matches!(
                    &expected.kind,
                    ExpressionTypeKind::Null | ExpressionTypeKind::Any
                )
            {
                return Ok(());
            }
            return Err(EvaluationError::new(
                EvaluationErrorKind::ContractViolation,
                format!(
                    "context produced null for non-nullable {}",
                    expected.describe()
                ),
            ));
        }
        if expected.kind == ExpressionTypeKind::Any {
            return self.validate_untyped_value_at(value, depth);
        }
        match (&expected.kind, value) {
            (ExpressionTypeKind::Bool, ExpressionValue::Bool(_))
            | (ExpressionTypeKind::Integer, ExpressionValue::Int(_) | ExpressionValue::UInt(_))
            | (
                ExpressionTypeKind::Number,
                ExpressionValue::Int(_) | ExpressionValue::UInt(_) | ExpressionValue::Float(_),
            ) => self.check_value_shallow(value),
            (ExpressionTypeKind::String, ExpressionValue::String(value)) => {
                self.check_string(value)
            }
            (ExpressionTypeKind::List(item_type), ExpressionValue::List(values)) => {
                for value in values {
                    self.visit_collection()?;
                    self.validate_runtime_value_at(value, item_type, depth + 1)?;
                }
                Ok(())
            }
            (ExpressionTypeKind::Object(fields), ExpressionValue::Object(values)) => {
                for (name, field_type) in fields {
                    if !field_type.optional && !values.contains_key(name) {
                        return Err(EvaluationError::new(
                            EvaluationErrorKind::ContractViolation,
                            format!("context object is missing required field {name:?}"),
                        ));
                    }
                }
                for (name, value) in values {
                    self.visit_collection()?;
                    self.check_string(name)?;
                    if let Some(field_type) = fields.get(name) {
                        self.validate_runtime_value_at(value, field_type, depth + 1)?;
                    } else {
                        self.validate_untyped_value_at(value, depth + 1)?;
                    }
                }
                Ok(())
            }
            _ => Err(EvaluationError::new(
                EvaluationErrorKind::ContractViolation,
                format!(
                    "context produced {} where {} was declared",
                    runtime_type_name(value),
                    expected.describe()
                ),
            )),
        }
    }

    fn validate_untyped_value_at(
        &mut self,
        value: &ExpressionValue,
        depth: usize,
    ) -> Result<(), EvaluationError> {
        self.check_runtime_depth(depth)?;
        match value {
            ExpressionValue::String(value) => self.check_string(value),
            ExpressionValue::Float(value) if !value.is_finite() => Err(EvaluationError::new(
                EvaluationErrorKind::ContractViolation,
                "context contains a non-finite float",
            )),
            ExpressionValue::List(values) => {
                for value in values {
                    self.visit_collection()?;
                    self.validate_untyped_value_at(value, depth + 1)?;
                }
                Ok(())
            }
            ExpressionValue::Object(values) => {
                for (name, value) in values {
                    self.visit_collection()?;
                    self.check_string(name)?;
                    self.validate_untyped_value_at(value, depth + 1)?;
                }
                Ok(())
            }
            ExpressionValue::Null
            | ExpressionValue::Bool(_)
            | ExpressionValue::Int(_)
            | ExpressionValue::UInt(_)
            | ExpressionValue::Float(_) => Ok(()),
        }
    }

    fn check_runtime_depth(&self, depth: usize) -> Result<(), EvaluationError> {
        if depth > self.program.limits.max_depth {
            return Err(EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "runtime value exceeds depth limit {}",
                    self.program.limits.max_depth
                ),
            ));
        }
        Ok(())
    }

    fn values_equal(
        &mut self,
        left: &ExpressionValue,
        right: &ExpressionValue,
    ) -> Result<bool, EvaluationError> {
        self.values_equal_at(left, right, 1)
    }

    fn values_equal_at(
        &mut self,
        left: &ExpressionValue,
        right: &ExpressionValue,
        depth: usize,
    ) -> Result<bool, EvaluationError> {
        self.check_runtime_depth(depth)?;
        match (left, right) {
            (ExpressionValue::Null, ExpressionValue::Null) => Ok(true),
            (ExpressionValue::Null, _) | (_, ExpressionValue::Null) => Ok(false),
            (ExpressionValue::Bool(left), ExpressionValue::Bool(right)) => Ok(left == right),
            (ExpressionValue::String(left), ExpressionValue::String(right)) => {
                self.check_string(left)?;
                self.check_string(right)?;
                Ok(left == right)
            }
            (left, right) if is_numeric(left) && is_numeric(right) => {
                Ok(self.numeric_ordering(left, right)?.is_eq())
            }
            (ExpressionValue::List(left), ExpressionValue::List(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for (left, right) in left.iter().zip(right) {
                    self.visit_collection()?;
                    if !self.values_equal_at(left, right, depth + 1)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (ExpressionValue::Object(left), ExpressionValue::Object(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for (name, left) in left {
                    self.visit_collection()?;
                    let Some(right) = right.get(name) else {
                        return Ok(false);
                    };
                    self.check_string(name)?;
                    if !self.values_equal_at(left, right, depth + 1)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(EvaluationError::new(
                EvaluationErrorKind::InvalidOperand,
                format!(
                    "cannot compare {} with {}",
                    runtime_type_name(left),
                    runtime_type_name(right)
                ),
            )),
        }
    }

    fn values_ordering(
        &mut self,
        left: &ExpressionValue,
        right: &ExpressionValue,
    ) -> Result<std::cmp::Ordering, EvaluationError> {
        match (left, right) {
            (ExpressionValue::String(left), ExpressionValue::String(right)) => {
                self.check_string(left)?;
                self.check_string(right)?;
                Ok(left.cmp(right))
            }
            (left, right) if is_numeric(left) && is_numeric(right) => {
                self.numeric_ordering(left, right)
            }
            _ => Err(EvaluationError::new(
                EvaluationErrorKind::InvalidOperand,
                format!(
                    "ordering is not defined for {} and {}",
                    runtime_type_name(left),
                    runtime_type_name(right)
                ),
            )),
        }
    }

    fn numeric_ordering(
        &self,
        left: &ExpressionValue,
        right: &ExpressionValue,
    ) -> Result<std::cmp::Ordering, EvaluationError> {
        use ExpressionValue::{Float, Int, UInt};
        match (left, right) {
            (Int(left), Int(right)) => Ok(left.cmp(right)),
            (UInt(left), UInt(right)) => Ok(left.cmp(right)),
            (Int(left), UInt(right)) => Ok(if *left < 0 {
                std::cmp::Ordering::Less
            } else {
                (*left as u64).cmp(right)
            }),
            (UInt(left), Int(right)) => Ok(if *right < 0 {
                std::cmp::Ordering::Greater
            } else {
                left.cmp(&(*right as u64))
            }),
            (Float(left), Float(right)) => finite_float_ordering(*left, *right),
            (Float(left), Int(right)) => finite_float_ordering(*left, exact_i64_as_f64(*right)?),
            (Int(left), Float(right)) => finite_float_ordering(exact_i64_as_f64(*left)?, *right),
            (Float(left), UInt(right)) => finite_float_ordering(*left, exact_u64_as_f64(*right)?),
            (UInt(left), Float(right)) => finite_float_ordering(exact_u64_as_f64(*left)?, *right),
            _ => Err(EvaluationError::new(
                EvaluationErrorKind::InvalidOperand,
                "numeric comparison received a non-numeric value",
            )),
        }
    }

    fn consume_operation(&mut self) -> Result<(), EvaluationError> {
        self.operations = self.operations.saturating_add(1);
        if self.operations > self.program.limits.max_operations {
            return Err(EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "expression exceeded operation limit {}",
                    self.program.limits.max_operations
                ),
            ));
        }
        Ok(())
    }

    fn visit_collection(&mut self) -> Result<(), EvaluationError> {
        self.collection_visits = self.collection_visits.saturating_add(1);
        if self.collection_visits > self.program.limits.max_collection_visits {
            return Err(EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "expression exceeded collection-visit limit {}",
                    self.program.limits.max_collection_visits
                ),
            ));
        }
        Ok(())
    }

    fn check_string(&mut self, value: &str) -> Result<(), EvaluationError> {
        if value.len() > self.program.limits.max_string_bytes {
            return Err(EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "string is {} bytes; limit is {}",
                    value.len(),
                    self.program.limits.max_string_bytes
                ),
            ));
        }
        self.string_bytes = self.string_bytes.checked_add(value.len()).ok_or_else(|| {
            EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                "runtime cumulative string-byte counter overflowed",
            )
        })?;
        if self.string_bytes > self.program.limits.max_string_bytes {
            return Err(EvaluationError::new(
                EvaluationErrorKind::LimitExceeded,
                format!(
                    "runtime value exceeds cumulative string-byte limit {}",
                    self.program.limits.max_string_bytes
                ),
            ));
        }
        Ok(())
    }

    fn check_value_shallow(&mut self, value: &ExpressionValue) -> Result<(), EvaluationError> {
        match value {
            ExpressionValue::String(value) => self.check_string(value),
            ExpressionValue::Float(value) if !value.is_finite() => Err(EvaluationError::new(
                EvaluationErrorKind::ContractViolation,
                "expression value contains a non-finite float",
            )),
            _ => Ok(()),
        }
    }

    fn invalid_operand(&self, message: impl Into<String>) -> EvaluationValue {
        EvaluationValue::error(EvaluationErrorKind::InvalidOperand, message)
    }
}

fn merge_deferred(slot: &mut Option<EvaluationValue>, candidate: EvaluationValue) {
    let candidate_priority = deferred_priority(&candidate);
    let current_priority = slot.as_ref().map_or(0, deferred_priority);
    if candidate_priority > current_priority {
        *slot = Some(candidate);
    }
}

fn deferred_priority(value: &EvaluationValue) -> u8 {
    match value {
        EvaluationValue::Error(_) => 3,
        EvaluationValue::Unknown(_) => 2,
        EvaluationValue::Missing(_) => 1,
        EvaluationValue::Value(_) => 0,
    }
}

fn is_numeric(value: &ExpressionValue) -> bool {
    matches!(
        value,
        ExpressionValue::Int(_) | ExpressionValue::UInt(_) | ExpressionValue::Float(_)
    )
}

fn finite_float_ordering(left: f64, right: f64) -> Result<std::cmp::Ordering, EvaluationError> {
    if !left.is_finite() || !right.is_finite() {
        return Err(EvaluationError::new(
            EvaluationErrorKind::InvalidOperand,
            "numeric comparison requires finite floating-point values",
        ));
    }
    left.partial_cmp(&right).ok_or_else(|| {
        EvaluationError::new(
            EvaluationErrorKind::InvalidOperand,
            "floating-point values are not comparable",
        )
    })
}

fn exact_i64_as_f64(value: i64) -> Result<f64, EvaluationError> {
    let converted = value as f64;
    if converted as i128 == value as i128 {
        Ok(converted)
    } else {
        Err(EvaluationError::new(
            EvaluationErrorKind::InvalidOperand,
            format!("integer {value} cannot be represented exactly as a float"),
        ))
    }
}

fn exact_u64_as_f64(value: u64) -> Result<f64, EvaluationError> {
    let converted = value as f64;
    if converted as u128 == value as u128 {
        Ok(converted)
    } else {
        Err(EvaluationError::new(
            EvaluationErrorKind::InvalidOperand,
            format!("integer {value} cannot be represented exactly as a float"),
        ))
    }
}

fn runtime_type_name(value: &ExpressionValue) -> &'static str {
    match value {
        ExpressionValue::Null => "null",
        ExpressionValue::Bool(_) => "bool",
        ExpressionValue::Int(_) | ExpressionValue::UInt(_) => "integer",
        ExpressionValue::Float(_) => "number",
        ExpressionValue::String(_) => "string",
        ExpressionValue::List(_) => "list",
        ExpressionValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::super::context::{
        ContextProvenance, ContextScope, ContextValue, FieldSchema, ObjectSchema, SemanticFormat,
    };
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct TestActivation {
        types: BTreeMap<FieldRef, ExpressionType>,
        values: BTreeMap<FieldRef, EvaluationValue>,
    }

    impl TestActivation {
        fn typed(mut self, field: FieldRef, value_type: ExpressionType) -> Self {
            self.types.insert(field, value_type);
            self
        }

        fn valued(mut self, field: FieldRef, value: EvaluationValue) -> Self {
            self.values.insert(field, value);
            self
        }
    }

    impl ExpressionSchemaResolver for TestActivation {
        fn resolve(&self, reference: &FieldRef) -> Result<ExpressionType, SchemaResolutionError> {
            self.types.get(reference).cloned().ok_or_else(|| {
                SchemaResolutionError::new(format!("unknown test reference {reference:?}"))
            })
        }
    }

    impl ExpressionValueResolver for TestActivation {
        fn resolve(&self, reference: &FieldRef) -> EvaluationValue {
            self.values
                .get(reference)
                .cloned()
                .unwrap_or_else(|| EvaluationValue::missing("test value was not supplied"))
        }
    }

    fn field(name: &str) -> FieldRef {
        FieldRef::step("source").field(name)
    }

    fn context_ref(field: FieldRef) -> ExpressionV1 {
        ExpressionV1::Ref {
            reference: ReferenceV1::Context { field },
        }
    }

    fn local_ref(binding: &str, path: &[&str]) -> ExpressionV1 {
        ExpressionV1::Ref {
            reference: ReferenceV1::Local {
                binding: binding.into(),
                path: path.iter().map(|segment| (*segment).into()).collect(),
            },
        }
    }

    fn literal(value: ExpressionValue) -> ExpressionV1 {
        ExpressionV1::Literal { value }
    }

    fn bool_literal(value: bool) -> ExpressionV1 {
        literal(ExpressionValue::Bool(value))
    }

    fn string_literal(value: &str) -> ExpressionV1 {
        literal(ExpressionValue::String(value.into()))
    }

    fn int_literal(value: i64) -> ExpressionV1 {
        literal(ExpressionValue::Int(value))
    }

    fn compare(
        operator: ComparisonOperator,
        left: ExpressionV1,
        right: ExpressionV1,
    ) -> ExpressionV1 {
        ExpressionV1::Compare {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn checked_rule(expression: ExpressionV1, activation: &TestActivation) -> CheckedExpressionV1 {
        check_rule(expression, activation, ExpressionLimits::default()).unwrap()
    }

    fn assert_limit(diagnostics: &[ExpressionDiagnostic]) {
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::LimitExceeded));
    }

    #[test]
    fn checker_reports_unknown_references_bad_roots_and_bad_operands() {
        let activation = TestActivation::default();
        let diagnostics = check_rule(
            context_ref(field("missing")),
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            diagnostics[0].code,
            ExpressionDiagnosticCode::UnknownReference
        );

        let diagnostics = check_rule(
            string_literal("not a rule"),
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::ExpectedBoolean));

        let diagnostics = check_rule(
            compare(
                ComparisonOperator::Equal,
                string_literal("1"),
                int_literal(1),
            ),
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::TypeMismatch));

        let diagnostics = check_rule(
            ExpressionV1::IsEmpty {
                expression: Box::new(int_literal(1)),
            },
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::TypeMismatch));
    }

    #[test]
    fn checker_enforces_structural_string_regex_and_quantifier_limits() {
        let activation = TestActivation::default();

        let limits = ExpressionLimits {
            max_nodes: 2,
            ..ExpressionLimits::default()
        };
        assert_limit(
            &check_rule(
                ExpressionV1::All {
                    expressions: vec![bool_literal(true), bool_literal(true)],
                },
                &activation,
                limits,
            )
            .unwrap_err(),
        );

        let limits = ExpressionLimits {
            max_depth: 2,
            ..ExpressionLimits::default()
        };
        assert_limit(
            &check_rule(
                ExpressionV1::Not {
                    expression: Box::new(ExpressionV1::Not {
                        expression: Box::new(bool_literal(true)),
                    }),
                },
                &activation,
                limits,
            )
            .unwrap_err(),
        );

        let limits = ExpressionLimits {
            max_string_bytes: 3,
            ..ExpressionLimits::default()
        };
        assert_limit(
            &check_value_expression(string_literal("four"), &activation, limits).unwrap_err(),
        );

        let diagnostics = check_rule(
            ExpressionV1::Matches {
                value: Box::new(string_literal("value")),
                pattern: "[".into(),
            },
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::InvalidRegex));

        let limits = ExpressionLimits {
            max_regex_pattern_bytes: 2,
            ..ExpressionLimits::default()
        };
        assert_limit(
            &check_rule(
                ExpressionV1::Matches {
                    value: Box::new(string_literal("value")),
                    pattern: "long".into(),
                },
                &activation,
                limits,
            )
            .unwrap_err(),
        );

        let limits = ExpressionLimits {
            max_quantifier_depth: 0,
            ..ExpressionLimits::default()
        };
        assert_limit(
            &check_rule(
                ExpressionV1::Quantifier {
                    quantifier: CollectionQuantifier::Any,
                    collection: Box::new(literal(ExpressionValue::List(vec![]))),
                    binding: "item".into(),
                    predicate: Box::new(bool_literal(true)),
                },
                &activation,
                limits,
            )
            .unwrap_err(),
        );
    }

    #[test]
    fn logical_operators_have_identity_short_circuit_and_deferred_semantics() {
        let activation = TestActivation::default();
        assert_eq!(
            checked_rule(
                ExpressionV1::All {
                    expressions: vec![],
                },
                &activation,
            )
            .evaluate_rule(&activation),
            RuleEvaluation::True
        );
        assert_eq!(
            checked_rule(
                ExpressionV1::Any {
                    expressions: vec![],
                },
                &activation,
            )
            .evaluate_rule(&activation),
            RuleEvaluation::False
        );
        assert_eq!(
            checked_rule(
                ExpressionV1::Not {
                    expression: Box::new(bool_literal(false)),
                },
                &activation,
            )
            .evaluate_rule(&activation),
            RuleEvaluation::True
        );

        let missing = field("missing");
        let unknown = field("unknown");
        let failed = field("failed");
        let activation = TestActivation::default()
            .typed(
                missing.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .typed(
                unknown.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .typed(
                failed.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .valued(missing.clone(), EvaluationValue::missing("absent"))
            .valued(unknown.clone(), EvaluationValue::unknown("not ready"))
            .valued(
                failed.clone(),
                EvaluationValue::error(EvaluationErrorKind::Internal, "failed"),
            );

        let all = ExpressionV1::All {
            expressions: vec![context_ref(missing.clone()), bool_literal(false)],
        };
        assert_eq!(
            checked_rule(all, &activation).evaluate_rule(&activation),
            RuleEvaluation::False
        );

        let any = ExpressionV1::Any {
            expressions: vec![context_ref(failed.clone()), bool_literal(true)],
        };
        assert_eq!(
            checked_rule(any, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );

        let deferred = ExpressionV1::Any {
            expressions: vec![
                context_ref(missing),
                context_ref(unknown),
                context_ref(failed),
                bool_literal(false),
            ],
        };
        assert!(matches!(
            checked_rule(deferred, &activation).evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::Internal,
                ..
            })
        ));
    }

    #[test]
    fn missing_null_unknown_and_error_are_never_collapsed() {
        let missing = field("missing");
        let nullable = field("nullable");
        let unknown = field("unknown");
        let failed = field("failed");
        let required = field("required");
        let activation = TestActivation::default()
            .typed(
                missing.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .typed(
                nullable.clone(),
                ExpressionType::nullable(ExpressionTypeKind::Bool),
            )
            .typed(
                unknown.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .typed(
                failed.clone(),
                ExpressionType::optional(ExpressionTypeKind::Bool),
            )
            .typed(
                required.clone(),
                ExpressionType::required(ExpressionTypeKind::Bool),
            )
            .valued(missing.clone(), EvaluationValue::missing("absent"))
            .valued(
                nullable.clone(),
                EvaluationValue::value(ExpressionValue::Null),
            )
            .valued(
                unknown.clone(),
                EvaluationValue::unknown("upstream not run"),
            )
            .valued(
                failed.clone(),
                EvaluationValue::error(EvaluationErrorKind::Internal, "boom"),
            )
            .valued(
                required.clone(),
                EvaluationValue::missing("broken contract"),
            );

        assert!(matches!(
            checked_rule(context_ref(missing.clone()), &activation).evaluate_rule(&activation),
            RuleEvaluation::Missing(_)
        ));
        assert_eq!(
            checked_rule(context_ref(nullable.clone()), &activation).evaluate_rule(&activation),
            RuleEvaluation::Null
        );
        assert!(matches!(
            checked_rule(context_ref(unknown), &activation).evaluate_rule(&activation),
            RuleEvaluation::Unknown(_)
        ));
        assert!(matches!(
            checked_rule(context_ref(failed), &activation).evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::Internal,
                ..
            })
        ));
        assert!(matches!(
            checked_rule(context_ref(required), &activation).evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::ContractViolation,
                ..
            })
        ));

        let exists_missing = ExpressionV1::Exists {
            reference: ReferenceV1::Context {
                field: missing.clone(),
            },
        };
        assert_eq!(
            checked_rule(exists_missing, &activation).evaluate_rule(&activation),
            RuleEvaluation::False
        );
        let exists_null = ExpressionV1::Exists {
            reference: ReferenceV1::Context {
                field: nullable.clone(),
            },
        };
        assert_eq!(
            checked_rule(exists_null, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );
        let is_null = ExpressionV1::IsNull {
            expression: Box::new(context_ref(nullable)),
        };
        assert_eq!(
            checked_rule(is_null, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );
        let missing_is_null = ExpressionV1::IsNull {
            expression: Box::new(context_ref(missing)),
        };
        assert!(matches!(
            checked_rule(missing_is_null, &activation).evaluate_rule(&activation),
            RuleEvaluation::Missing(_)
        ));
    }

    #[test]
    fn strings_regex_and_empty_predicates_are_unicode_safe() {
        let activation = TestActivation::default();
        let expression = ExpressionV1::All {
            expressions: vec![
                ExpressionV1::Contains {
                    value: Box::new(string_literal("привет, мир")),
                    needle: Box::new(string_literal("мир")),
                },
                ExpressionV1::StartsWith {
                    value: Box::new(string_literal("репозиторий")),
                    prefix: Box::new(string_literal("репо")),
                },
                ExpressionV1::EndsWith {
                    value: Box::new(string_literal("日本語")),
                    suffix: Box::new(string_literal("語")),
                },
                ExpressionV1::Matches {
                    value: Box::new(string_literal("Привет42")),
                    pattern: r"^Привет\d+$".into(),
                },
                ExpressionV1::IsEmpty {
                    expression: Box::new(string_literal("")),
                },
                ExpressionV1::IsEmpty {
                    expression: Box::new(literal(ExpressionValue::List(vec![]))),
                },
                ExpressionV1::IsEmpty {
                    expression: Box::new(literal(ExpressionValue::Object(BTreeMap::new()))),
                },
            ],
        };
        assert_eq!(
            checked_rule(expression, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );

        let input = field("input");
        let activation = TestActivation::default()
            .typed(
                input.clone(),
                ExpressionType::required(ExpressionTypeKind::String),
            )
            .valued(
                input.clone(),
                EvaluationValue::value(ExpressionValue::String("1234".into())),
            );
        let limits = ExpressionLimits {
            max_regex_input_bytes: 3,
            ..ExpressionLimits::default()
        };
        let checked = check_rule(
            ExpressionV1::Matches {
                value: Box::new(context_ref(input)),
                pattern: r"\d+".into(),
            },
            &activation,
            limits,
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));
    }

    #[test]
    fn comparisons_and_membership_are_typed_and_bounded() {
        let activation = TestActivation::default();
        let expression = ExpressionV1::All {
            expressions: vec![
                compare(
                    ComparisonOperator::LessThan,
                    int_literal(1),
                    literal(ExpressionValue::Float(2.0)),
                ),
                compare(
                    ComparisonOperator::GreaterThanOrEqual,
                    string_literal("beta"),
                    string_literal("alpha"),
                ),
                compare(
                    ComparisonOperator::Equal,
                    literal(ExpressionValue::Null),
                    literal(ExpressionValue::Null),
                ),
                ExpressionV1::In {
                    needle: Box::new(int_literal(2)),
                    collection: Box::new(literal(ExpressionValue::List(vec![
                        ExpressionValue::Int(1),
                        ExpressionValue::Int(2),
                        ExpressionValue::Int(3),
                    ]))),
                },
            ],
        };
        assert_eq!(
            checked_rule(expression, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );

        let checked = checked_rule(
            compare(
                ComparisonOperator::Equal,
                literal(ExpressionValue::Int(9_007_199_254_740_993)),
                literal(ExpressionValue::Float(9_007_199_254_740_992.0)),
            ),
            &activation,
        );
        assert!(matches!(
            checked.evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::InvalidOperand,
                ..
            })
        ));

        let limits = ExpressionLimits {
            max_collection_visits: 1,
            ..ExpressionLimits::default()
        };
        let checked = check_rule(
            ExpressionV1::In {
                needle: Box::new(int_literal(3)),
                collection: Box::new(literal(ExpressionValue::List(vec![
                    ExpressionValue::Int(1),
                    ExpressionValue::Int(2),
                ]))),
            },
            &activation,
            limits,
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));
    }

    #[test]
    fn quantifiers_bind_typed_items_and_preserve_empty_and_deferred_results() {
        fn repository(id: i64, name: &str) -> ExpressionValue {
            ExpressionValue::Object(BTreeMap::from([
                ("id".into(), ExpressionValue::Int(id)),
                ("name".into(), ExpressionValue::String(name.into())),
            ]))
        }

        let repositories = literal(ExpressionValue::List(vec![
            repository(1, "alpha"),
            repository(2, "beta"),
        ]));
        let any = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(repositories.clone()),
            binding: "repository".into(),
            predicate: Box::new(compare(
                ComparisonOperator::Equal,
                local_ref("repository", &["id"]),
                int_literal(2),
            )),
        };
        let all = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::All,
            collection: Box::new(repositories.clone()),
            binding: "repository".into(),
            predicate: Box::new(ExpressionV1::Contains {
                value: Box::new(local_ref("repository", &["name"])),
                needle: Box::new(string_literal("a")),
            }),
        };
        let none = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::None,
            collection: Box::new(repositories),
            binding: "repository".into(),
            predicate: Box::new(compare(
                ComparisonOperator::Equal,
                local_ref("repository", &["id"]),
                int_literal(3),
            )),
        };
        let activation = TestActivation::default();
        assert_eq!(
            checked_rule(any, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );
        assert_eq!(
            checked_rule(all, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );
        assert_eq!(
            checked_rule(none, &activation).evaluate_rule(&activation),
            RuleEvaluation::True
        );

        for (quantifier, expected) in [
            (CollectionQuantifier::Any, RuleEvaluation::False),
            (CollectionQuantifier::All, RuleEvaluation::True),
            (CollectionQuantifier::None, RuleEvaluation::True),
        ] {
            let expression = ExpressionV1::Quantifier {
                quantifier,
                collection: Box::new(literal(ExpressionValue::List(vec![]))),
                binding: "item".into(),
                predicate: Box::new(bool_literal(true)),
            };
            assert_eq!(
                checked_rule(expression, &activation).evaluate_rule(&activation),
                expected
            );
        }

        let items = field("items");
        let item_type = ExpressionType::required(ExpressionTypeKind::Object(BTreeMap::from([(
            "enabled".into(),
            ExpressionType::optional(ExpressionTypeKind::Bool),
        )])));
        let activation = TestActivation::default()
            .typed(items.clone(), ExpressionType::list(item_type))
            .valued(
                items.clone(),
                EvaluationValue::value(ExpressionValue::List(vec![ExpressionValue::Object(
                    BTreeMap::new(),
                )])),
            );
        let deferred = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(context_ref(items)),
            binding: "item".into(),
            predicate: Box::new(local_ref("item", &["enabled"])),
        };
        assert!(matches!(
            checked_rule(deferred, &activation).evaluate_rule(&activation),
            RuleEvaluation::Missing(_)
        ));
    }

    #[test]
    fn checker_rejects_invalid_quantifier_bindings_and_local_fields() {
        let activation = TestActivation::default();
        let nested = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(literal(ExpressionValue::List(vec![ExpressionValue::List(
                vec![ExpressionValue::Int(1)],
            )]))),
            binding: "item".into(),
            predicate: Box::new(ExpressionV1::Quantifier {
                quantifier: CollectionQuantifier::Any,
                collection: Box::new(local_ref("item", &[])),
                binding: "item".into(),
                predicate: Box::new(bool_literal(true)),
            }),
        };
        let diagnostics = check_rule(nested, &activation, ExpressionLimits::default()).unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::InvalidBinding));

        let bad_field = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(literal(ExpressionValue::List(vec![
                ExpressionValue::Object(BTreeMap::from([(
                    "known".into(),
                    ExpressionValue::Bool(true),
                )])),
            ]))),
            binding: "item".into(),
            predicate: Box::new(local_ref("item", &["unknown"])),
        };
        let diagnostics =
            check_rule(bad_field, &activation, ExpressionLimits::default()).unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == ExpressionDiagnosticCode::UnknownField));
    }

    #[test]
    fn runtime_validates_schema_contracts_and_operation_budget() {
        let flag = field("flag");
        let activation = TestActivation::default()
            .typed(
                flag.clone(),
                ExpressionType::required(ExpressionTypeKind::Bool),
            )
            .valued(
                flag.clone(),
                EvaluationValue::value(ExpressionValue::String("not bool".into())),
            );
        assert!(matches!(
            checked_rule(context_ref(flag), &activation).evaluate_rule(&activation),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::ContractViolation,
                ..
            })
        ));

        let limits = ExpressionLimits {
            max_operations: 1,
            ..ExpressionLimits::default()
        };
        let checked = check_rule(
            ExpressionV1::Not {
                expression: Box::new(bool_literal(false)),
            },
            &TestActivation::default(),
            limits,
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate_rule(&TestActivation::default()),
            RuleEvaluation::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));

        let values = field("values");
        let activation = TestActivation::default()
            .typed(
                values.clone(),
                ExpressionType::list(ExpressionType::required(ExpressionTypeKind::String)),
            )
            .valued(
                values.clone(),
                EvaluationValue::value(ExpressionValue::List(vec![ExpressionValue::Int(1)])),
            );
        let checked = check_value_expression(
            context_ref(values),
            &activation,
            ExpressionLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate(&activation),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::ContractViolation,
                ..
            })
        ));
    }

    #[test]
    fn reference_visitors_support_template_step_id_rewrites() {
        let first = FieldRef::step("fetch").field("ready");
        let second = FieldRef::loop_item("loop").field("enabled");
        let mut expression = ExpressionV1::All {
            expressions: vec![
                context_ref(first),
                ExpressionV1::Exists {
                    reference: ReferenceV1::Context { field: second },
                },
                ExpressionV1::Quantifier {
                    quantifier: CollectionQuantifier::Any,
                    collection: Box::new(literal(ExpressionValue::List(vec![
                        ExpressionValue::Bool(true),
                    ]))),
                    binding: "local".into(),
                    predicate: Box::new(local_ref("local", &[])),
                },
            ],
        };

        let mut before = Vec::new();
        expression.visit_context_references(|field| before.push(field.clone()));
        assert_eq!(before.len(), 2);

        expression.visit_context_references_mut(|field| match &mut field.scope {
            ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                *step_id = format!("template/{step_id}");
            }
            ContextScope::Scenario => {}
        });

        let mut after = Vec::new();
        expression.visit_context_references(|field| after.push(field.clone()));
        assert_eq!(
            after
                .iter()
                .map(|field| match &field.scope {
                    ContextScope::Step { step_id } | ContextScope::LoopItem { step_id } => {
                        step_id.as_str()
                    }
                    ContextScope::Scenario => "scenario",
                })
                .collect::<Vec<_>>(),
            vec!["template/fetch", "template/loop"]
        );
    }

    #[test]
    fn expression_ast_round_trips_without_string_evaluation() {
        let expression = ExpressionV1::Quantifier {
            quantifier: CollectionQuantifier::Any,
            collection: Box::new(context_ref(FieldRef::step("list").field("repositories"))),
            binding: "repository".into(),
            predicate: Box::new(ExpressionV1::Matches {
                value: Box::new(local_ref("repository", &["name"])),
                pattern: "^[a-z0-9-]+$".into(),
            }),
        };
        let serialized = serde_json::to_value(&expression).unwrap();
        assert_eq!(serialized["op"], "quantifier");
        assert_eq!(
            serde_json::from_value::<ExpressionV1>(serialized).unwrap(),
            expression
        );

        let invalid =
            json!({"op": "literal", "value": {"type": "bool", "value": true}, "extra": true});
        assert!(serde_json::from_value::<ExpressionV1>(invalid).is_err());
    }

    #[test]
    fn context_store_adapter_supports_roots_optional_fields_and_blocks_secrets() {
        let schema = ObjectSchema::new("test.context@1")
            .with_field("ready", FieldSchema::required(ContextType::Boolean))
            .with_field(
                "url",
                FieldSchema::required(ContextType::string(SemanticFormat::GitUrl)),
            )
            .with_field("note", FieldSchema::optional(ContextType::STRING))
            .with_field(
                "token",
                FieldSchema::optional(ContextType::STRING).sensitive(Sensitivity::Secret),
            );
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "source".into(),
            },
            ContextValue::new(
                json!({
                    "ready": true,
                    "url": "https://github.com/example/repository.git",
                    "token": "secret"
                }),
                ContextProvenance::step("source"),
            )
            .with_schema(schema),
        );

        let ready = field("ready");
        let checked = check_rule(context_ref(ready), &store, ExpressionLimits::default()).unwrap();
        assert_eq!(checked.evaluate_rule(&store), RuleEvaluation::True);

        let optional = field("note");
        let checked = check_rule(
            ExpressionV1::Exists {
                reference: ReferenceV1::Context { field: optional },
            },
            &store,
            ExpressionLimits::default(),
        )
        .unwrap();
        assert_eq!(checked.evaluate_rule(&store), RuleEvaluation::False);

        let secret_diagnostics = check_value_expression(
            context_ref(field("token")),
            &store,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            secret_diagnostics[0].code,
            ExpressionDiagnosticCode::UnknownReference
        );

        let root_diagnostics = check_value_expression(
            context_ref(FieldRef::step("source")),
            &store,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            root_diagnostics[0].code,
            ExpressionDiagnosticCode::UnknownReference
        );

        assert!(matches!(
            ExpressionValueResolver::resolve(&store, &FieldRef::step("not-created").field("x")),
            EvaluationValue::Unknown(_)
        ));

        let public_schema = ObjectSchema::new("test.public-context@1")
            .with_field("ready", FieldSchema::required(ContextType::Boolean));
        let mut public_store = ContextStore::default();
        public_store.insert(
            ContextScope::Step {
                step_id: "public".into(),
            },
            ContextValue::new(json!({"ready": true}), ContextProvenance::step("public"))
                .with_schema(public_schema),
        );
        let root = FieldRef::step("public");
        let checked = check_value_expression(
            context_ref(root),
            &public_store,
            ExpressionLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate(&public_store),
            EvaluationValue::Value(ExpressionValue::Object(_))
        ));
    }

    #[test]
    fn checker_and_evaluator_snapshot_each_context_reference_once() {
        use std::cell::Cell;

        struct ChangingSchema {
            calls: Cell<usize>,
        }

        impl ExpressionSchemaResolver for ChangingSchema {
            fn resolve(
                &self,
                _reference: &FieldRef,
            ) -> Result<ExpressionType, SchemaResolutionError> {
                let call = self.calls.get();
                self.calls.set(call + 1);
                Ok(if call == 0 {
                    ExpressionType::required(ExpressionTypeKind::Bool)
                } else {
                    ExpressionType::required(ExpressionTypeKind::String)
                })
            }
        }

        struct ChangingValue {
            calls: Cell<usize>,
        }

        impl ExpressionValueResolver for ChangingValue {
            fn resolve(&self, _reference: &FieldRef) -> EvaluationValue {
                let call = self.calls.get();
                self.calls.set(call + 1);
                EvaluationValue::value(ExpressionValue::Bool(call == 0))
            }
        }

        let repeated = field("repeated");
        let expression = ExpressionV1::All {
            expressions: vec![context_ref(repeated.clone()), context_ref(repeated)],
        };
        let schema = ChangingSchema {
            calls: Cell::new(0),
        };
        let checked = check_rule(expression, &schema, ExpressionLimits::default()).unwrap();
        assert_eq!(schema.calls.get(), 1);

        let values = ChangingValue {
            calls: Cell::new(0),
        };
        assert_eq!(checked.evaluate_rule(&values), RuleEvaluation::True);
        assert_eq!(values.calls.get(), 1);
    }

    #[test]
    fn checker_bounds_context_schema_projection_before_recursive_clone() {
        let mut nested = ContextType::Boolean;
        for _ in 0..64 {
            nested = ContextType::array(nested);
        }
        let mut deep_store = ContextStore::default();
        deep_store.insert(
            ContextScope::Scenario,
            ContextValue::new(json!([]), ContextProvenance::step("deep-schema")).with_type(nested),
        );
        let limits = ExpressionLimits {
            max_depth: 4,
            ..ExpressionLimits::default()
        };
        let diagnostics =
            check_value_expression(context_ref(FieldRef::scenario()), &deep_store, limits)
                .unwrap_err();
        assert_limit(&diagnostics);

        let mut wide = ObjectSchema::new("wide-schema");
        for index in 0..32 {
            wide = wide.with_field(
                format!("field_{index}"),
                FieldSchema::optional(ContextType::Boolean),
            );
        }
        let mut wide_store = ContextStore::default();
        wide_store.insert(
            ContextScope::Scenario,
            ContextValue::new(json!({}), ContextProvenance::step("wide-schema")).with_schema(wide),
        );
        let limits = ExpressionLimits {
            max_collection_visits: 4,
            ..ExpressionLimits::default()
        };
        let diagnostics =
            check_value_expression(context_ref(FieldRef::scenario()), &wide_store, limits)
                .unwrap_err();
        assert_limit(&diagnostics);

        let names = ObjectSchema::new("string-budget")
            .with_field("aaaa", FieldSchema::optional(ContextType::Boolean))
            .with_field("bbbb", FieldSchema::optional(ContextType::Boolean))
            .with_field("cccc", FieldSchema::optional(ContextType::Boolean));
        let mut names_store = ContextStore::default();
        names_store.insert(
            ContextScope::Scenario,
            ContextValue::new(json!({}), ContextProvenance::step("string-budget"))
                .with_schema(names),
        );
        let limits = ExpressionLimits {
            max_string_bytes: 8,
            ..ExpressionLimits::default()
        };
        let diagnostics =
            check_value_expression(context_ref(FieldRef::scenario()), &names_store, limits)
                .unwrap_err();
        assert_limit(&diagnostics);
    }

    #[test]
    fn evaluator_bounds_json_conversion_before_recursive_clone() {
        let mut deep = json!(true);
        for _ in 0..64 {
            deep = json!([deep]);
        }
        let mut deep_store = ContextStore::default();
        deep_store.insert(
            ContextScope::Scenario,
            ContextValue::new(deep, ContextProvenance::step("deep-value"))
                .with_type(ContextType::Any),
        );
        let limits = ExpressionLimits {
            max_depth: 4,
            ..ExpressionLimits::default()
        };
        let checked =
            check_value_expression(context_ref(FieldRef::scenario()), &deep_store, limits).unwrap();
        assert!(matches!(
            checked.evaluate(&deep_store),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));

        let mut wide_store = ContextStore::default();
        wide_store.insert(
            ContextScope::Scenario,
            ContextValue::new(
                serde_json::Value::Array((0..32).map(|value| json!(value)).collect()),
                ContextProvenance::step("wide-value"),
            )
            .with_type(ContextType::array(ContextType::Integer)),
        );
        let limits = ExpressionLimits {
            max_collection_visits: 4,
            ..ExpressionLimits::default()
        };
        let checked =
            check_value_expression(context_ref(FieldRef::scenario()), &wide_store, limits).unwrap();
        assert!(matches!(
            checked.evaluate(&wide_store),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));

        let mut strings_store = ContextStore::default();
        strings_store.insert(
            ContextScope::Scenario,
            ContextValue::new(
                json!(["aaaa", "bbbb", "cccc"]),
                ContextProvenance::step("string-value"),
            )
            .with_type(ContextType::array(ContextType::STRING)),
        );
        let limits = ExpressionLimits {
            max_string_bytes: 8,
            ..ExpressionLimits::default()
        };
        let checked =
            check_value_expression(context_ref(FieldRef::scenario()), &strings_store, limits)
                .unwrap();
        assert!(matches!(
            checked.evaluate(&strings_store),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));
    }

    #[test]
    fn bounded_context_adapters_keep_normal_github_arrays_valid_and_block_nested_secrets() {
        let repository = ContextType::object(
            ObjectSchema::new("repository")
                .with_field("name", FieldSchema::required(ContextType::STRING))
                .with_field("private", FieldSchema::required(ContextType::Boolean)),
        );
        let schema = ObjectSchema::new("github")
            .with_field(
                "repositories",
                FieldSchema::required(ContextType::array(repository)),
            )
            .with_field(
                "credentials",
                FieldSchema::optional(ContextType::object(
                    ObjectSchema::new("credentials").with_field(
                        "token",
                        FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Secret),
                    ),
                )),
            );
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Scenario,
            ContextValue::new(
                json!({
                    "repositories": [
                        { "name": "api", "private": true },
                        { "name": "web", "private": false }
                    ]
                }),
                ContextProvenance::step("github"),
            )
            .with_schema(schema),
        );

        let repositories = FieldRef::scenario().field("repositories");
        let checked = check_value_expression(
            context_ref(repositories),
            &store,
            ExpressionLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            checked.evaluate(&store),
            EvaluationValue::Value(ExpressionValue::List(values)) if values.len() == 2
        ));

        let diagnostics = check_value_expression(
            context_ref(FieldRef::scenario()),
            &store,
            ExpressionLimits::default(),
        )
        .unwrap_err();
        assert_eq!(
            diagnostics[0].code,
            ExpressionDiagnosticCode::UnknownReference
        );
        assert!(diagnostics[0].message.contains("secret context field"));
    }

    #[test]
    fn checked_limits_are_forwarded_to_bounded_resolver_methods() {
        use std::cell::Cell;

        struct BoundedActivation {
            schema_limit: Cell<usize>,
            value_limit: Cell<usize>,
        }

        impl ExpressionSchemaResolver for BoundedActivation {
            fn resolve(
                &self,
                _reference: &FieldRef,
            ) -> Result<ExpressionType, SchemaResolutionError> {
                panic!("checker should call resolve_schema_bounded")
            }

            fn resolve_schema_bounded(
                &self,
                _reference: &FieldRef,
                limits: ExpressionLimits,
            ) -> Result<ExpressionType, SchemaResolutionError> {
                self.schema_limit.set(limits.max_depth);
                Ok(ExpressionType::bool())
            }
        }

        impl ExpressionValueResolver for BoundedActivation {
            fn resolve(&self, _reference: &FieldRef) -> EvaluationValue {
                panic!("evaluator should call resolve_value_bounded")
            }

            fn resolve_value_bounded(
                &self,
                _reference: &FieldRef,
                limits: ExpressionLimits,
            ) -> EvaluationValue {
                self.value_limit.set(limits.max_depth);
                EvaluationValue::value(ExpressionValue::Bool(true))
            }
        }

        let activation = BoundedActivation {
            schema_limit: Cell::new(0),
            value_limit: Cell::new(0),
        };
        let limits = ExpressionLimits {
            max_depth: 7,
            ..ExpressionLimits::default()
        };
        let checked = check_rule(context_ref(field("bounded")), &activation, limits).unwrap();
        assert_eq!(activation.schema_limit.get(), 7);
        assert_eq!(checked.evaluate_rule(&activation), RuleEvaluation::True);
        assert_eq!(activation.value_limit.get(), 7);
    }

    #[test]
    fn arbitrary_runtime_values_are_deeply_bounded() {
        let anything = field("anything");
        let activation = TestActivation::default()
            .typed(
                anything.clone(),
                ExpressionType::required(ExpressionTypeKind::Any),
            )
            .valued(
                anything.clone(),
                EvaluationValue::value(ExpressionValue::List(vec![ExpressionValue::List(vec![
                    ExpressionValue::List(vec![ExpressionValue::Int(1)]),
                ])])),
            );
        let limits = ExpressionLimits {
            max_depth: 2,
            ..ExpressionLimits::default()
        };
        let checked = check_value_expression(context_ref(anything), &activation, limits).unwrap();
        assert!(matches!(
            checked.evaluate(&activation),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));

        let object = field("object");
        let activation = TestActivation::default()
            .typed(object.clone(), ExpressionType::object(BTreeMap::new()))
            .valued(
                object.clone(),
                EvaluationValue::value(ExpressionValue::Object(BTreeMap::from([(
                    "extra".into(),
                    ExpressionValue::List(vec![ExpressionValue::Int(1)]),
                )]))),
            );
        let limits = ExpressionLimits {
            max_collection_visits: 1,
            ..ExpressionLimits::default()
        };
        let checked = check_value_expression(context_ref(object), &activation, limits).unwrap();
        assert!(matches!(
            checked.evaluate(&activation),
            EvaluationValue::Error(EvaluationError {
                kind: EvaluationErrorKind::LimitExceeded,
                ..
            })
        ));
    }
}
