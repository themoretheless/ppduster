//! Resolution and application of typed action-input bindings.
//!
//! Bindings are data, not executable source. The resolver can read only the
//! supplied [`ContextStore`], enforces the consumer's input schema, propagates
//! sensitivity, and applies bounded structural templates without shell or I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::str::FromStr;

use serde_json::Value;
use thiserror::Error;

use super::block::definition_for_action;
use super::context::{
    Binding, ContextPathSegment, ContextStore, ContextType, FieldRef, ObjectSchema,
    ResolvedSchemaOwned, SemanticFormat, Sensitivity, TemplatePart,
};
use super::task::Step;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingLimits {
    pub max_bindings: usize,
    pub max_path_segments: usize,
    pub max_template_parts: usize,
    pub max_rendered_bytes: usize,
}

impl Default for BindingLimits {
    fn default() -> Self {
        Self {
            max_bindings: 128,
            max_path_segments: 32,
            max_template_parts: 256,
            max_rendered_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBinding {
    pub value: Value,
    pub sources: Vec<FieldRef>,
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BindingError {
    #[error("binding target {0:?} is empty")]
    EmptyTarget(String),
    #[error("binding target {target:?} has an invalid JSON Pointer escape")]
    InvalidPointer { target: String },
    #[error("binding target {target:?} exceeds {limit} path segments")]
    PathLimit { target: String, limit: usize },
    #[error("action {action} has no declared input field at {target}")]
    UnknownInput { action: String, target: String },
    #[error(
        "binding target {target:?} indexes an action input array; bind the whole array instead"
    )]
    IndexedInputTarget { target: String },
    #[error("context reference {reference:?} has no declared schema")]
    MissingSourceSchema { reference: FieldRef },
    #[error("context reference {reference:?} cannot be resolved: {message}")]
    MissingSource {
        reference: FieldRef,
        message: String,
    },
    #[error("context reference {reference:?} violates its declared contract: {message}")]
    InvalidSourceValue {
        reference: FieldRef,
        message: String,
    },
    #[error(
        "context reference {reference:?} has incompatible type {actual:?}; expected {expected:?}"
    )]
    TypeMismatch {
        reference: FieldRef,
        expected: ContextType,
        actual: ContextType,
    },
    #[error("binding value for {target} violates its input contract: {message}")]
    InvalidValue { target: String, message: String },
    #[error("secret context reference {reference:?} cannot flow into a non-secret input")]
    SecretFlow { reference: FieldRef },
    #[error("legacy textual template contains placeholders; migrate it to structural parts")]
    LegacyTemplate,
    #[error("template has {found} parts; limit is {limit}")]
    TemplatePartLimit { found: usize, limit: usize },
    #[error("template field {reference:?} is null or not a scalar")]
    NonScalarTemplateField { reference: FieldRef },
    #[error("rendered template exceeds {limit} bytes")]
    RenderedSizeLimit { limit: usize },
    #[error("step contains {found} bindings; limit is {limit}")]
    BindingCountLimit { found: usize, limit: usize },
    #[error("two binding targets normalize to {target}")]
    DuplicateTarget { target: String },
    #[error("cannot write binding target {target}: {message}")]
    Patch { target: String, message: String },
    #[error("bound step is invalid: {0}")]
    InvalidStep(String),
}

/// Resolve one binding against a consumer field contract.
pub fn resolve_binding(
    binding: &Binding,
    expected: &ResolvedSchemaOwned,
    context: &ContextStore,
    limits: BindingLimits,
) -> Result<ResolvedBinding, BindingError> {
    let mut resolved = match binding {
        Binding::Literal { value } => ResolvedBinding {
            value: value.clone(),
            sources: Vec::new(),
            sensitivity: Sensitivity::Public,
        },
        Binding::Field { field } => resolve_field(field, expected, context)?,
        Binding::Template { template } => {
            if template.contains("{{") || template.contains("}}") {
                return Err(BindingError::LegacyTemplate);
            }
            ResolvedBinding {
                value: Value::String(template.clone()),
                sources: Vec::new(),
                sensitivity: Sensitivity::Public,
            }
        }
        Binding::Interpolated { parts } => resolve_interpolated(parts, expected, context, limits)?,
    };

    // Enforce information-flow policy before any value validation. Semantic
    // validators may include the rejected scalar in their diagnostic, so a
    // secret must never reach them when the target is not secret-capable.
    if resolved.sensitivity.is_secret() && expected.sensitivity != Sensitivity::Secret {
        let reference = resolved
            .sources
            .first()
            .cloned()
            .unwrap_or_else(FieldRef::scenario);
        return Err(BindingError::SecretFlow { reference });
    }
    validate_value_for_schema(&resolved.value, expected).map_err(|message| {
        let message =
            if resolved.sensitivity.is_secret() || expected.sensitivity == Sensitivity::Secret {
                "secret binding value does not satisfy its input contract".into()
            } else {
                message
            };
        BindingError::InvalidValue {
            target: "$binding".into(),
            message,
        }
    })?;
    // A structural template always creates a new derived value. Preserve all
    // source references and their maximum sensitivity in the returned value.
    resolved.sources.sort();
    resolved.sources.dedup();
    Ok(resolved)
}

/// Validate a literal editor value against one declared action-input field.
///
/// This is the side-effect-free validation entry point used by schema-driven
/// editors before they persist a `Binding::Literal`. It applies the same
/// structural, nullable, and semantic-format rules as runtime materialization.
pub fn validate_literal_binding(
    value: &Value,
    expected: &super::context::FieldSchema,
) -> Result<(), String> {
    validate_value_for_schema(
        value,
        &ResolvedSchemaOwned {
            value_type: expected.value_type.clone(),
            required: true,
            nullable: expected.nullable,
            sensitivity: expected.sensitivity,
            allowed_values: expected.allowed_values.clone(),
        },
    )
}

fn resolve_field(
    field: &FieldRef,
    expected: &ResolvedSchemaOwned,
    context: &ContextStore,
) -> Result<ResolvedBinding, BindingError> {
    let source_schema =
        context
            .resolve_type_owned(field)
            .ok_or_else(|| BindingError::MissingSourceSchema {
                reference: field.clone(),
            })?;
    let value = context
        .resolve(field)
        .map_err(|error| BindingError::MissingSource {
            reference: field.clone(),
            message: error.to_string(),
        })?;
    // Direct field bindings can reject a forbidden secret flow before even a
    // structural type diagnostic. Apart from being fail-closed, this keeps
    // future richer type errors from accidentally disclosing source data.
    if value.sensitivity.is_secret() && expected.sensitivity != Sensitivity::Secret {
        return Err(BindingError::SecretFlow {
            reference: field.clone(),
        });
    }
    validate_source_value(field, value.value, &source_schema, value.sensitivity)?;
    if !expected
        .value_type
        .is_assignable_from(&source_schema.value_type)
    {
        return Err(BindingError::TypeMismatch {
            reference: field.clone(),
            expected: expected.value_type.clone(),
            actual: source_schema.value_type,
        });
    }
    Ok(ResolvedBinding {
        value: value.value.clone(),
        sources: vec![field.clone()],
        sensitivity: value.sensitivity,
    })
}

fn resolve_interpolated(
    parts: &[TemplatePart],
    expected: &ResolvedSchemaOwned,
    context: &ContextStore,
    limits: BindingLimits,
) -> Result<ResolvedBinding, BindingError> {
    if parts.len() > limits.max_template_parts {
        return Err(BindingError::TemplatePartLimit {
            found: parts.len(),
            limit: limits.max_template_parts,
        });
    }
    let mut rendered = String::new();
    let mut sources = Vec::new();
    let mut sensitivity = Sensitivity::Public;
    for part in parts {
        match part {
            TemplatePart::Literal { value } => append_bounded(&mut rendered, value, limits)?,
            TemplatePart::Field { field } => {
                let source_schema = context.resolve_type_owned(field).ok_or_else(|| {
                    BindingError::MissingSourceSchema {
                        reference: field.clone(),
                    }
                })?;
                let resolved =
                    context
                        .resolve(field)
                        .map_err(|error| BindingError::MissingSource {
                            reference: field.clone(),
                            message: error.to_string(),
                        })?;
                if resolved.sensitivity.is_secret() && expected.sensitivity != Sensitivity::Secret {
                    return Err(BindingError::SecretFlow {
                        reference: field.clone(),
                    });
                }
                validate_source_value(field, resolved.value, &source_schema, resolved.sensitivity)?;
                let scalar = match resolved.value {
                    Value::String(value) => value.clone(),
                    Value::Bool(value) => value.to_string(),
                    Value::Number(value) => value.to_string(),
                    Value::Null | Value::Array(_) | Value::Object(_) => {
                        return Err(BindingError::NonScalarTemplateField {
                            reference: field.clone(),
                        })
                    }
                };
                append_bounded(&mut rendered, &scalar, limits)?;
                sources.push(field.clone());
                sensitivity = sensitivity.combine(resolved.sensitivity);
            }
        }
    }
    Ok(ResolvedBinding {
        value: Value::String(rendered),
        sources,
        sensitivity,
    })
}

fn validate_source_value(
    reference: &FieldRef,
    value: &Value,
    schema: &ResolvedSchemaOwned,
    sensitivity: Sensitivity,
) -> Result<(), BindingError> {
    validate_value_for_schema(value, schema).map_err(|message| {
        let message = if sensitivity.is_secret() {
            "secret context value does not satisfy its declared contract".into()
        } else {
            message
        };
        BindingError::InvalidSourceValue {
            reference: reference.clone(),
            message,
        }
    })
}

fn append_bounded(
    rendered: &mut String,
    value: &str,
    limits: BindingLimits,
) -> Result<(), BindingError> {
    if rendered.len().saturating_add(value.len()) > limits.max_rendered_bytes {
        return Err(BindingError::RenderedSizeLimit {
            limit: limits.max_rendered_bytes,
        });
    }
    rendered.push_str(value);
    Ok(())
}

/// Resolve and patch all bindings into a cloned [`Step`]. Binding keys are
/// RFC 6901 JSON Pointers (`/checksum/sha256`) or convenient dotted paths
/// (`checksum.sha256`). Only paths declared by the action's input schema are
/// writable.
pub fn materialize_step(
    step: &Step,
    bindings: &BTreeMap<String, Binding>,
    context: &ContextStore,
    limits: BindingLimits,
) -> Result<Step, BindingError> {
    if bindings.len() > limits.max_bindings {
        return Err(BindingError::BindingCountLimit {
            found: bindings.len(),
            limit: limits.max_bindings,
        });
    }
    let definition = definition_for_action(&step.action);
    let mut serialized =
        serde_json::to_value(step).map_err(|error| BindingError::InvalidStep(error.to_string()))?;
    let mut seen = BTreeSet::new();
    for (target, binding) in bindings {
        let segments = parse_binding_target(target, limits)?;
        let canonical = display_target(&segments);
        if segments
            .iter()
            .any(|segment| matches!(segment, ContextPathSegment::Index { .. }))
        {
            return Err(BindingError::IndexedInputTarget { target: canonical });
        }
        if !seen.insert(canonical.clone()) {
            return Err(BindingError::DuplicateTarget { target: canonical });
        }
        let expected = definition
            .input_schema
            .resolve_input_target_owned(&segments)
            .ok_or_else(|| BindingError::UnknownInput {
                action: definition.kind.id().into(),
                target: canonical.clone(),
            })?;
        let resolved = match resolve_binding(binding, &expected, context, limits) {
            Ok(resolved) => resolved,
            Err(BindingError::MissingSource { .. }) if !expected.required => {
                // Optional inputs use missing-as-omit semantics. The action's
                // declared literal/default remains in place, which makes
                // guarded and partially-populated contexts composable without
                // conflating missing with null.
                continue;
            }
            Err(BindingError::InvalidValue { message, .. }) => {
                return Err(BindingError::InvalidValue {
                    target: canonical,
                    message,
                });
            }
            Err(other) => return Err(other),
        };
        set_value_at_path(&mut serialized, &segments, resolved.value).map_err(|message| {
            BindingError::Patch {
                target: canonical,
                message,
            }
        })?;
    }
    let materialized: Step = serde_json::from_value(serialized)
        .map_err(|error| BindingError::InvalidStep(error.to_string()))?;
    materialized.validate().map_err(BindingError::InvalidStep)?;
    Ok(materialized)
}

pub fn parse_binding_target(
    target: &str,
    limits: BindingLimits,
) -> Result<Vec<ContextPathSegment>, BindingError> {
    if target.trim().is_empty() || target == "/" {
        return Err(BindingError::EmptyTarget(target.into()));
    }
    let segments = if let Some(pointer) = target.strip_prefix('/') {
        pointer
            .split('/')
            .map(|segment| decode_pointer_segment(segment, target))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        target
            .split('.')
            .map(|segment| {
                if segment.is_empty() {
                    Err(BindingError::EmptyTarget(target.into()))
                } else {
                    Ok(segment.into())
                }
            })
            .collect::<Result<Vec<String>, _>>()?
    };
    if segments.len() > limits.max_path_segments {
        return Err(BindingError::PathLimit {
            target: target.into(),
            limit: limits.max_path_segments,
        });
    }
    Ok(segments
        .into_iter()
        .map(|segment| {
            segment
                .parse::<usize>()
                .map(ContextPathSegment::index)
                .unwrap_or_else(|_| ContextPathSegment::field(segment))
        })
        .collect())
}

fn decode_pointer_segment(segment: &str, target: &str) -> Result<String, BindingError> {
    let mut decoded = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(character) = chars.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => {
                return Err(BindingError::InvalidPointer {
                    target: target.into(),
                })
            }
        }
    }
    if decoded.is_empty() {
        return Err(BindingError::EmptyTarget(target.into()));
    }
    Ok(decoded)
}

fn display_target(segments: &[ContextPathSegment]) -> String {
    let mut result = String::new();
    for segment in segments {
        result.push('/');
        match segment {
            ContextPathSegment::Field { name } => {
                result.push_str(&name.replace('~', "~0").replace('/', "~1"));
            }
            ContextPathSegment::Index { index } => result.push_str(&index.to_string()),
        }
    }
    result
}

fn set_value_at_path(
    root: &mut Value,
    segments: &[ContextPathSegment],
    value: Value,
) -> Result<(), String> {
    let Some((last, parents)) = segments.split_last() else {
        return Err("target has no path segments".into());
    };
    let mut current = root;
    for (position, segment) in parents.iter().enumerate() {
        let next = parents.get(position + 1).unwrap_or(last);
        current = match segment {
            ContextPathSegment::Field { name } => {
                let object = current
                    .as_object_mut()
                    .ok_or_else(|| format!("parent of {name:?} is not an object"))?;
                let child = object.entry(name.clone()).or_insert(Value::Null);
                if child.is_null() {
                    *child = match next {
                        ContextPathSegment::Field { .. } => Value::Object(Default::default()),
                        ContextPathSegment::Index { .. } => Value::Array(Vec::new()),
                    };
                }
                child
            }
            ContextPathSegment::Index { index } => current
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .ok_or_else(|| format!("missing array index {index}"))?,
        };
    }
    match last {
        ContextPathSegment::Field { name } => {
            current
                .as_object_mut()
                .ok_or_else(|| format!("parent of {name:?} is not an object"))?
                .insert(name.clone(), value);
        }
        ContextPathSegment::Index { index } => {
            let slot = current
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .ok_or_else(|| format!("missing array index {index}"))?;
            *slot = value;
        }
    }
    Ok(())
}

fn validate_value_for_schema(value: &Value, expected: &ResolvedSchemaOwned) -> Result<(), String> {
    let field = super::context::FieldSchema {
        value_type: expected.value_type.clone(),
        required: true,
        nullable: expected.nullable,
        description: None,
        sensitivity: expected.sensitivity,
        allowed_values: expected.allowed_values.clone(),
    };
    let wrapper = ObjectSchema::new("ppduster.binding.value@1").with_field("value", field);
    wrapper
        .validate_value(&serde_json::json!({ "value": value }))
        .map_err(|error| error.to_string())?;
    validate_semantic_value(value, &expected.value_type)
}

fn validate_semantic_value(value: &Value, expected: &ContextType) -> Result<(), String> {
    match (value, expected) {
        (Value::Null, _) => Ok(()),
        (
            Value::String(value),
            ContextType::String {
                format: Some(format),
            },
        ) => validate_string_format(value, *format),
        (Value::Array(values), ContextType::Array { items }) => {
            for value in values {
                validate_semantic_value(value, items)?;
            }
            Ok(())
        }
        (Value::Object(values), ContextType::Object { schema }) => {
            for (name, field) in &schema.fields {
                if let Some(value) = values.get(name) {
                    validate_semantic_value(value, &field.value_type)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_string_format(value: &str, format: SemanticFormat) -> Result<(), String> {
    let valid = match format {
        SemanticFormat::Path | SemanticFormat::FilePath | SemanticFormat::DirectoryPath => {
            !value.is_empty() && !value.contains('\0')
        }
        SemanticFormat::Url => valid_url(value),
        SemanticFormat::GitUrl => valid_git_url(value),
        SemanticFormat::SecretRef | SemanticFormat::Duration => !value.trim().is_empty(),
        SemanticFormat::Sha256 => {
            value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        }
        SemanticFormat::DateTime => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
        SemanticFormat::Email => {
            let mut parts = value.split('@');
            parts.next().is_some_and(|part| !part.is_empty())
                && parts.next().is_some_and(|part| part.contains('.'))
                && parts.next().is_none()
        }
        SemanticFormat::Hostname => valid_hostname(value),
        SemanticFormat::IpAddress => IpAddr::from_str(value).is_ok(),
        SemanticFormat::Uuid => valid_uuid(value),
        SemanticFormat::GitRef => valid_git_ref(value),
        SemanticFormat::RepositoryName => valid_repository_name(value),
        SemanticFormat::Identifier => valid_identifier(value),
        SemanticFormat::OpaqueId => !value.is_empty() && !value.contains('\0'),
    };
    if valid {
        Ok(())
    } else {
        Err(format!("{value:?} is not a valid {format:?}"))
    }
}

fn valid_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        && !rest.is_empty()
        && !value.chars().any(char::is_whitespace)
}

fn valid_git_url(value: &str) -> bool {
    valid_url(value)
        || value
            .split_once(':')
            .is_some_and(|(host, path)| host.contains('@') && !path.is_empty())
}

fn valid_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn valid_git_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.ends_with(['.', '/'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

fn valid_repository_name(value: &str) -> bool {
    !value.is_empty()
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.chars().all(|character| {
                    character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
        })
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::block::{block_definition, default_step, ActionKind};
    use crate::automation::context::{
        ContextProvenance, ContextScope, ContextValue, FieldSchema, ObjectSchema,
    };
    use crate::automation::task::{Action, AuthPolicy, ElevationPolicy};

    fn github_context_with_names(owner: &str, name: &str, full_name: &str) -> ContextStore {
        let schema = block_definition(ActionKind::GithubListRepositories).output_schema;
        let value = serde_json::json!({
            "github": {
                "account": { "login": "octocat" },
                "repositories": [{
                    "id": "R_123",
                    "owner": owner,
                    "name": name,
                    "full_name": full_name,
                    "https_url": "https://github.com/acme/service.git",
                    "ssh_url": "git@github.com:acme/service.git",
                    "default_branch": "main",
                    "private": false,
                    "archived": false
                }]
            }
        });
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "list".into(),
            },
            ContextValue::new(value, ContextProvenance::step("list")).with_schema(schema),
        );
        store
    }

    fn github_context() -> ContextStore {
        github_context_with_names("acme", "service", "acme/service")
    }

    #[test]
    fn github_repository_collection_accepts_opaque_node_ids() {
        let schema = block_definition(ActionKind::GithubListRepositories).output_schema;
        let value = serde_json::json!({
            "github": {
                "account": { "login": "octocat" },
                "repositories": [{
                    "id": "MDEwOlJlcG9zaXRvcnkxMjM0NTY3ODk=",
                    "owner": "acme",
                    "name": "service",
                    "full_name": "acme/service",
                    "https_url": "https://github.com/acme/service.git",
                    "ssh_url": "git@github.com:acme/service.git",
                    "default_branch": "main",
                    "private": false,
                    "archived": false
                }]
            }
        });
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "list".into(),
            },
            ContextValue::new(value, ContextProvenance::step("list")).with_schema(schema),
        );
        let collection = FieldRef::step("list").field("github").field("repositories");
        let expected = ResolvedSchemaOwned {
            value_type: ContextType::array(ContextType::Any),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Secret,
            allowed_values: Vec::new(),
        };

        let resolved = resolve_binding(
            &Binding::field(collection),
            &expected,
            &store,
            BindingLimits::default(),
        )
        .expect("GitHub node IDs must be treated as opaque strings");

        assert_eq!(resolved.value[0]["id"], "MDEwOlJlcG9zaXRvcnkxMjM0NTY3ODk=");
    }

    fn clone_step() -> Step {
        Step {
            id: "clone".into(),
            name: "Clone".into(),
            bindings: BTreeMap::new(),
            auth: AuthPolicy::None,
            check: None,
            dangerous: false,
            allow_elevation: ElevationPolicy::Forbidden,
            when: None,
            require: None,
            action: Action::GitCloneIfMissing {
                repo: "https://github.com/placeholder/repository.git".into(),
                dest: "/tmp/placeholder/repository".into(),
                branch: Some("main".into()),
            },
        }
    }

    #[test]
    fn typed_fields_and_structural_templates_materialize_an_action() {
        let store = github_context();
        let repository = FieldRef::step("list")
            .field("github")
            .field("repositories")
            .index(0);
        let bindings = BTreeMap::from([
            (
                "/repo".into(),
                Binding::field(repository.clone().field("https_url")),
            ),
            (
                "/dest".into(),
                Binding::interpolated([
                    TemplatePart::literal("/tmp/"),
                    TemplatePart::field(repository.clone().field("owner")),
                    TemplatePart::literal("/"),
                    TemplatePart::field(repository.clone().field("name")),
                ]),
            ),
            (
                "/branch".into(),
                Binding::field(repository.field("default_branch")),
            ),
        ]);

        let materialized =
            materialize_step(&clone_step(), &bindings, &store, BindingLimits::default()).unwrap();
        let Action::GitCloneIfMissing { repo, dest, branch } = materialized.action else {
            panic!("expected clone action")
        };
        assert_eq!(repo, "https://github.com/acme/service.git");
        assert_eq!(dest, "/tmp/acme/service");
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn nested_binding_materializes_an_optional_object_from_null() {
        let step = default_step(ActionKind::InspectPath, "inspect").unwrap();
        let bindings = BTreeMap::from([(
            "expect.exists".into(),
            Binding::literal(serde_json::Value::Bool(false)),
        )]);

        let materialized = materialize_step(
            &step,
            &bindings,
            &ContextStore::default(),
            BindingLimits::default(),
        )
        .unwrap();
        let Action::InspectPath(action) = materialized.action else {
            panic!("expected inspect-path action")
        };
        assert_eq!(
            action.expect.and_then(|expectation| expectation.exists),
            Some(false)
        );
    }

    #[test]
    fn missing_source_omits_an_optional_input_override() {
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "source".into(),
            },
            ContextValue::new(serde_json::json!({}), ContextProvenance::step("source"))
                .with_schema(ObjectSchema::new("optional-source").with_field(
                    "branch",
                    FieldSchema::optional(ContextType::string(SemanticFormat::GitRef)),
                )),
        );
        let bindings = BTreeMap::from([(
            "branch".into(),
            Binding::field(FieldRef::step("source").field("branch")),
        )]);

        let materialized =
            materialize_step(&clone_step(), &bindings, &store, BindingLimits::default()).unwrap();
        let Action::GitCloneIfMissing { branch, .. } = materialized.action else {
            panic!("expected clone action")
        };
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn missing_source_schema_never_silently_omits_an_optional_override() {
        let field = FieldRef::scenario().field("branch");
        let bindings = BTreeMap::from([("branch".into(), Binding::field(field.clone()))]);

        let error = materialize_step(
            &clone_step(),
            &bindings,
            &ContextStore::default(),
            BindingLimits::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BindingError::MissingSourceSchema { reference } if reference == field
        ));
    }

    #[test]
    fn indexed_action_input_targets_are_rejected_before_patching() {
        let step = default_step(ActionKind::RunCommand, "run").unwrap();
        let bindings = BTreeMap::from([("args.0".into(), Binding::literal("--version"))]);

        let error = materialize_step(
            &step,
            &bindings,
            &ContextStore::default(),
            BindingLimits::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            BindingError::IndexedInputTarget { ref target } if target == "/args/0"
        ));
    }

    #[test]
    fn literal_editor_validation_uses_runtime_semantic_contracts() {
        let inputs = block_definition(ActionKind::GitCloneIfMissing).input_schema;
        let repo = inputs.field("repo").unwrap();
        assert!(validate_literal_binding(&serde_json::json!("not a git url"), repo).is_err());
        assert!(validate_literal_binding(
            &serde_json::json!("https://github.com/acme/service.git"),
            repo,
        )
        .is_ok());

        let branch = inputs.field("branch").unwrap();
        assert!(validate_literal_binding(&serde_json::Value::Null, branch).is_ok());
    }

    #[test]
    fn repository_name_requires_safe_non_empty_path_segments() {
        for valid in [
            "acme",
            "service-api_2",
            "acme/service.v2",
            "org/team/repository",
            "организация/репозиторий",
        ] {
            assert!(
                valid_repository_name(valid),
                "expected {valid:?} to be a valid repository name"
            );
        }

        for invalid in [
            "",
            ".",
            "..",
            "/repository",
            "repository/",
            "owner//repository",
            "owner/./repository",
            "owner/../repository",
            "owner/repository/..",
        ] {
            assert!(
                !valid_repository_name(invalid),
                "expected {invalid:?} to be rejected"
            );
        }
    }

    #[test]
    fn opaque_id_accepts_base64_without_promising_an_identifier_charset() {
        assert!(
            validate_string_format("MDEwOlJlcG9zaXRvcnkxMjM0NTY=", SemanticFormat::OpaqueId)
                .is_ok()
        );
        assert!(validate_string_format("", SemanticFormat::OpaqueId).is_err());
        assert!(validate_string_format("node\0id", SemanticFormat::OpaqueId).is_err());
    }

    #[test]
    fn interpolated_binding_accepts_normal_repository_source_fields() {
        let store =
            github_context_with_names("организация", "сервис.v2", "организация/команда/сервис.v2");
        let repository = FieldRef::step("list")
            .field("github")
            .field("repositories")
            .index(0);
        let expected = ResolvedSchemaOwned {
            value_type: ContextType::string(SemanticFormat::DirectoryPath),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        let binding = Binding::interpolated([
            TemplatePart::literal("/tmp/"),
            TemplatePart::field(repository.clone().field("owner")),
            TemplatePart::literal("/"),
            TemplatePart::field(repository.clone().field("name")),
            TemplatePart::literal("/"),
            TemplatePart::field(repository.field("full_name")),
        ]);

        let resolved = resolve_binding(&binding, &expected, &store, BindingLimits::default())
            .expect("normal owner, name, and full_name must remain usable");
        assert_eq!(
            resolved.value,
            Value::String("/tmp/организация/сервис.v2/организация/команда/сервис.v2".into())
        );
    }

    #[test]
    fn interpolated_binding_rejects_traversal_in_repository_source_fields() {
        let cases = [
            ("owner", "..", "service", "acme/service"),
            ("name", "acme", "../service", "acme/service"),
            ("full_name", "acme", "service", "acme/../service"),
        ];
        let expected = ResolvedSchemaOwned {
            value_type: ContextType::string(SemanticFormat::DirectoryPath),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };

        for (field_name, owner, name, full_name) in cases {
            let store = github_context_with_names(owner, name, full_name);
            let field = FieldRef::step("list")
                .field("github")
                .field("repositories")
                .index(0)
                .field(field_name);
            let binding = Binding::interpolated([
                TemplatePart::literal("/tmp/"),
                TemplatePart::field(field.clone()),
            ]);

            let error = resolve_binding(&binding, &expected, &store, BindingLimits::default())
                .expect_err("traversal-like repository fields must fail before interpolation");
            assert!(
                matches!(
                    error,
                    BindingError::InvalidSourceValue { ref reference, .. } if reference == &field
                ),
                "unexpected error for {field_name}: {error}"
            );
        }
    }

    #[test]
    fn semantic_format_prevents_repository_name_from_becoming_git_url() {
        let store = github_context();
        let bindings = BTreeMap::from([(
            "/repo".into(),
            Binding::field(
                FieldRef::step("list")
                    .field("github")
                    .field("repositories")
                    .index(0)
                    .field("name"),
            ),
        )]);
        let error = materialize_step(&clone_step(), &bindings, &store, BindingLimits::default())
            .unwrap_err();
        assert!(matches!(error, BindingError::TypeMismatch { .. }));
    }

    #[test]
    fn legacy_placeholder_templates_fail_closed() {
        let expected = ResolvedSchemaOwned {
            value_type: ContextType::STRING,
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        let error = resolve_binding(
            &Binding::template("{{repository.url}}"),
            &expected,
            &ContextStore::default(),
            BindingLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error, BindingError::LegacyTemplate);
    }

    fn secret_url_context(secret: &str) -> ContextStore {
        let schema = ObjectSchema::new("secret-url-source").with_field(
            "url",
            super::super::context::FieldSchema::required(ContextType::string(
                SemanticFormat::GitUrl,
            ))
            .sensitive(Sensitivity::Secret),
        );
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "secret-source".into(),
            },
            ContextValue::new(
                serde_json::json!({ "url": secret }),
                ContextProvenance::step("secret-source"),
            )
            .with_schema(schema),
        );
        store
    }

    #[test]
    fn secret_flow_precedes_semantic_validation_and_never_echoes_value() {
        let secret = "plaintext-secret-that-is-not-a-url";
        let binding = Binding::field(FieldRef::step("secret-source").field("url"));
        let public_git_url = ResolvedSchemaOwned {
            value_type: ContextType::string(SemanticFormat::GitUrl),
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        let error = resolve_binding(
            &binding,
            &public_git_url,
            &secret_url_context(secret),
            BindingLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, BindingError::SecretFlow { .. }));
        assert!(!error.to_string().contains(secret));

        let secret_git_url = ResolvedSchemaOwned {
            sensitivity: Sensitivity::Secret,
            ..public_git_url
        };
        let error = resolve_binding(
            &binding,
            &secret_git_url,
            &secret_url_context(secret),
            BindingLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, BindingError::InvalidSourceValue { .. }));
        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("secret context value"));
    }

    #[test]
    fn binding_a_public_parent_or_root_cannot_launder_a_nested_secret() {
        let payload = ObjectSchema::new("payload").with_field(
            "token",
            super::super::context::FieldSchema::required(ContextType::STRING)
                .sensitive(Sensitivity::Secret),
        );
        let root = ObjectSchema::new("root").with_field(
            "payload",
            super::super::context::FieldSchema::required(ContextType::object(payload)),
        );
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Step {
                step_id: "nested".into(),
            },
            ContextValue::new(
                serde_json::json!({ "payload": { "token": "nested-secret" } }),
                ContextProvenance::step("nested"),
            )
            .with_schema(root),
        );
        let public_object = ResolvedSchemaOwned {
            value_type: ContextType::Any,
            required: true,
            nullable: false,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        };
        for reference in [
            FieldRef::step("nested"),
            FieldRef::step("nested").field("payload"),
        ] {
            let error = resolve_binding(
                &Binding::field(reference),
                &public_object,
                &store,
                BindingLimits::default(),
            )
            .unwrap_err();
            assert!(matches!(error, BindingError::SecretFlow { .. }));
            assert!(!error.to_string().contains("nested-secret"));
        }
    }
}
