use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// Current wire version for context schemas and stored context values.
pub const CONTEXT_SCHEMA_VERSION: u32 = 1;

fn context_schema_version() -> u32 {
    CONTEXT_SCHEMA_VERSION
}

/// A semantic refinement for a scalar context value.
///
/// Formats do not change the JSON representation. They let the editor offer
/// compatible bindings (for example, a Git URL for a repository input) while
/// keeping the runtime value a normal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticFormat {
    Path,
    FilePath,
    DirectoryPath,
    Url,
    GitUrl,
    SecretRef,
    Sha256,
    DateTime,
    Duration,
    Email,
    Hostname,
    IpAddress,
    Uuid,
    GitRef,
    RepositoryName,
    Identifier,
    OpaqueIdentifier,
}

/// Structural type information shared by the runner, expression checker, and
/// visual context picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ContextType {
    Any,
    Null,
    Boolean,
    Integer,
    Number,
    String {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<SemanticFormat>,
    },
    Array {
        items: Box<ContextType>,
    },
    Object {
        schema: Box<ObjectSchema>,
    },
}

impl ContextType {
    pub const STRING: Self = Self::String { format: None };

    pub const fn string(format: SemanticFormat) -> Self {
        Self::String {
            format: Some(format),
        }
    }

    pub fn array(items: ContextType) -> Self {
        Self::Array {
            items: Box::new(items),
        }
    }

    pub fn object(schema: ObjectSchema) -> Self {
        Self::Object {
            schema: Box::new(schema),
        }
    }

    /// Returns whether a value of `actual` type can be supplied where `self`
    /// is expected.
    pub fn is_assignable_from(&self, actual: &Self) -> bool {
        match (self, actual) {
            (Self::Any, _) => true,
            (Self::Null, Self::Null)
            | (Self::Boolean, Self::Boolean)
            | (Self::Integer, Self::Integer)
            | (Self::Number, Self::Number | Self::Integer) => true,
            (Self::String { format: expected }, Self::String { format: actual }) => {
                semantic_format_accepts(*expected, *actual)
            }
            (Self::Array { items: expected }, Self::Array { items: actual }) => {
                expected.is_assignable_from(actual)
            }
            (Self::Object { schema: expected }, Self::Object { schema: actual }) => {
                expected.is_assignable_from(actual)
            }
            _ => false,
        }
    }

    pub fn infer(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Boolean,
            Value::Number(number) if number.is_i64() || number.is_u64() => Self::Integer,
            Value::Number(_) => Self::Number,
            Value::String(_) => Self::STRING,
            Value::Array(values) => {
                let mut inferred = values.iter().map(Self::infer);
                let items = inferred.next().map_or(Self::Any, |first| {
                    inferred.fold(first, |current, next| common_type(&current, &next))
                });
                Self::array(items)
            }
            Value::Object(values) => {
                let fields = values
                    .iter()
                    .map(|(name, value)| (name.clone(), FieldSchema::required(Self::infer(value))))
                    .collect();
                Self::object(ObjectSchema::anonymous(fields))
            }
        }
    }
}

fn semantic_format_accepts(
    expected: Option<SemanticFormat>,
    actual: Option<SemanticFormat>,
) -> bool {
    match (expected, actual) {
        (None, _) => true,
        (Some(expected), Some(actual)) if expected == actual => true,
        (Some(SemanticFormat::Url), Some(SemanticFormat::GitUrl)) => true,
        (Some(SemanticFormat::Path), Some(SemanticFormat::FilePath))
        | (Some(SemanticFormat::Path), Some(SemanticFormat::DirectoryPath)) => true,
        _ => false,
    }
}

fn common_type(left: &ContextType, right: &ContextType) -> ContextType {
    if left == right {
        return left.clone();
    }
    if matches!((left, right), (ContextType::Integer, ContextType::Number))
        || matches!((left, right), (ContextType::Number, ContextType::Integer))
    {
        return ContextType::Number;
    }
    ContextType::Any
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sensitivity {
    #[default]
    Public,
    Internal,
    Confidential,
    Secret,
}

impl Sensitivity {
    pub fn combine(self, other: Self) -> Self {
        self.max(other)
    }

    pub fn is_secret(self) -> bool {
        matches!(self, Self::Secret)
    }
}

/// Schema for one named object field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSchema {
    #[serde(rename = "type")]
    pub value_type: ContextType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "is_public")]
    pub sensitivity: Sensitivity,
    /// Closed set of literal values accepted by this field. An empty set
    /// means that every value satisfying `type` is allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_values: Vec<Value>,
}

fn is_public(value: &Sensitivity) -> bool {
    matches!(value, Sensitivity::Public)
}

impl FieldSchema {
    pub fn required(value_type: ContextType) -> Self {
        Self {
            value_type,
            required: true,
            nullable: false,
            description: None,
            sensitivity: Sensitivity::Public,
            allowed_values: Vec::new(),
        }
    }

    pub fn optional(value_type: ContextType) -> Self {
        Self {
            required: false,
            ..Self::required(value_type)
        }
    }

    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn sensitive(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    pub fn with_allowed_values<I, V>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<Value>,
    {
        self.allowed_values = values.into_iter().map(Into::into).collect();
        self
    }

    pub fn accepts(&self, actual: &Self) -> bool {
        (!self.required || actual.required)
            && (field_accepts_null(self) || !field_may_be_null(actual))
            && self.value_type.is_assignable_from(&actual.value_type)
            && allowed_values_accept(&self.allowed_values, &actual.allowed_values)
    }
}

fn allowed_values_accept(expected: &[Value], actual: &[Value]) -> bool {
    expected.is_empty()
        || (!actual.is_empty()
            && actual
                .iter()
                .all(|value| expected.iter().any(|allowed| allowed == value)))
}

fn type_accepts_null(value_type: &ContextType) -> bool {
    matches!(value_type, ContextType::Any | ContextType::Null)
}

fn field_accepts_null(field: &FieldSchema) -> bool {
    field.nullable || type_accepts_null(&field.value_type)
}

fn field_may_be_null(field: &FieldSchema) -> bool {
    field.nullable || type_accepts_null(&field.value_type)
}

/// Contract for object keys not listed in [`ObjectSchema::fields`].
///
/// Boolean serialization preserves schema-v1 compatibility (`false` means
/// closed, `true` means untyped/open). A context type is the new typed form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AdditionalProperties {
    #[default]
    Forbidden,
    Any,
    Typed(ContextType),
}

impl AdditionalProperties {
    pub fn typed(value_type: ContextType) -> Self {
        if matches!(value_type, ContextType::Any) {
            Self::Any
        } else {
            Self::Typed(value_type)
        }
    }

    pub fn value_type(&self) -> Option<&ContextType> {
        static ANY: ContextType = ContextType::Any;
        match self {
            Self::Forbidden => None,
            Self::Any => Some(&ANY),
            Self::Typed(value_type) => Some(value_type),
        }
    }
}

impl Serialize for AdditionalProperties {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Forbidden => false.serialize(serializer),
            Self::Any => true.serialize(serializer),
            Self::Typed(value_type) => value_type.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AdditionalProperties {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Bool(false) => Ok(Self::Forbidden),
            Value::Bool(true) => Ok(Self::Any),
            value => serde_json::from_value::<ContextType>(value)
                .map(Self::typed)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// Versioned structural schema for an object-shaped context value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectSchema {
    #[serde(default = "context_schema_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldSchema>,
    #[serde(default)]
    pub additional_fields: AdditionalProperties,
}

impl ObjectSchema {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            version: CONTEXT_SCHEMA_VERSION,
            id: Some(id.into()),
            fields: BTreeMap::new(),
            additional_fields: AdditionalProperties::Forbidden,
        }
    }

    pub fn anonymous(fields: BTreeMap<String, FieldSchema>) -> Self {
        Self {
            version: CONTEXT_SCHEMA_VERSION,
            id: None,
            fields,
            additional_fields: AdditionalProperties::Forbidden,
        }
    }

    pub fn with_field(mut self, name: impl Into<String>, field: FieldSchema) -> Self {
        self.fields.insert(name.into(), field);
        self
    }

    pub fn with_additional_fields(mut self, value_type: ContextType) -> Self {
        self.additional_fields = AdditionalProperties::typed(value_type);
        self
    }

    pub fn allowing_additional_fields(mut self) -> Self {
        self.additional_fields = AdditionalProperties::Any;
        self
    }

    pub fn field(&self, name: &str) -> Option<&FieldSchema> {
        self.fields.get(name)
    }

    pub fn resolve(&self, segments: &[ContextPathSegment]) -> Option<ResolvedSchema<'_>> {
        let (first, rest) = segments.split_first()?;
        let ContextPathSegment::Field { name } = first else {
            return None;
        };
        if let Some(field) = self.fields.get(name) {
            resolve_field_schema(
                field,
                rest,
                field.required,
                field.nullable,
                field.sensitivity,
            )
        } else {
            resolve_type_schema(
                self.additional_fields.value_type()?,
                rest,
                false,
                false,
                Sensitivity::Public,
            )
        }
    }

    /// Resolve a path to owned type information. Unlike the borrowed resolver,
    /// this also represents the object root when `segments` is empty.
    pub fn resolve_owned(&self, segments: &[ContextPathSegment]) -> Option<ResolvedSchemaOwned> {
        if segments.is_empty() {
            return Some(ResolvedSchemaOwned {
                value_type: ContextType::object(self.clone()),
                required: true,
                nullable: false,
                sensitivity: aggregate_object_sensitivity(self),
                allowed_values: Vec::new(),
            });
        }
        self.resolve(segments).map(ResolvedSchemaOwned::from)
    }

    /// Resolve an explicitly configured action-input target.
    ///
    /// This differs from resolving an output reference. A nullable or optional
    /// ancestor may make reading `parent.child` indeterminate, but writing a
    /// binding to that child materializes the ancestor object. Consequently
    /// ancestor nullability does not make the child value nullable; only the
    /// leaf field controls whether a written value may be null. Optionality is
    /// preserved so a missing source can explicitly mean "omit this optional
    /// override and keep the action default".
    pub fn resolve_input_target_owned(
        &self,
        segments: &[ContextPathSegment],
    ) -> Option<ResolvedSchemaOwned> {
        let mut resolved = self.resolve_owned(segments)?;
        resolved.nullable = input_target_nullable(self, segments)?;
        Some(resolved)
    }

    pub fn is_assignable_from(&self, actual: &Self) -> bool {
        let declared_fields_are_compatible = self.fields.iter().all(|(name, expected)| {
            if let Some(actual) = actual.fields.get(name) {
                return expected.accepts(actual);
            }
            if expected.required {
                return false;
            }
            actual
                .additional_fields
                .value_type()
                .is_none_or(|actual| field_accepts_additional_type(expected, actual))
        });
        if !declared_fields_are_compatible {
            return false;
        }

        let known_extras_are_compatible = actual
            .fields
            .iter()
            .filter(|(name, _)| !self.fields.contains_key(*name))
            .all(|(_, actual)| {
                self.additional_fields
                    .value_type()
                    .is_some_and(|expected| additional_type_accepts_field(expected, actual))
            });
        if !known_extras_are_compatible {
            return false;
        }

        match (
            self.additional_fields.value_type(),
            actual.additional_fields.value_type(),
        ) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(expected), Some(actual)) => expected.is_assignable_from(actual),
        }
    }

    /// Validate a JSON object against this versioned schema.
    pub fn validate_value(&self, value: &Value) -> Result<(), SchemaValidationError> {
        validate_schema_version(self.version, &[])?;
        validate_object_value(self, value, &mut Vec::new())
    }
}

fn input_target_nullable(schema: &ObjectSchema, segments: &[ContextPathSegment]) -> Option<bool> {
    let (first, rest) = segments.split_first()?;
    let ContextPathSegment::Field { name } = first else {
        return None;
    };
    let value_type = if let Some(field) = schema.fields.get(name) {
        if rest.is_empty() {
            return Some(field.nullable);
        }
        &field.value_type
    } else {
        let value_type = schema.additional_fields.value_type()?;
        if rest.is_empty() {
            return Some(type_accepts_null(value_type));
        }
        value_type
    };
    input_type_target_nullable(value_type, rest)
}

fn input_type_target_nullable(
    value_type: &ContextType,
    segments: &[ContextPathSegment],
) -> Option<bool> {
    let (first, rest) = segments.split_first()?;
    match (first, value_type) {
        (ContextPathSegment::Field { name }, ContextType::Object { schema }) => {
            let child = if let Some(field) = schema.fields.get(name) {
                if rest.is_empty() {
                    return Some(field.nullable);
                }
                &field.value_type
            } else {
                let child = schema.additional_fields.value_type()?;
                if rest.is_empty() {
                    return Some(type_accepts_null(child));
                }
                child
            };
            input_type_target_nullable(child, rest)
        }
        (ContextPathSegment::Index { .. }, ContextType::Array { items }) => {
            if rest.is_empty() {
                Some(type_accepts_null(items))
            } else {
                input_type_target_nullable(items, rest)
            }
        }
        _ => None,
    }
}

fn field_accepts_additional_type(expected: &FieldSchema, actual: &ContextType) -> bool {
    expected.value_type.is_assignable_from(actual)
        && (field_accepts_null(expected) || !type_accepts_null(actual))
}

fn additional_type_accepts_field(expected: &ContextType, actual: &FieldSchema) -> bool {
    expected.is_assignable_from(&actual.value_type)
        && (type_accepts_null(expected) || !field_may_be_null(actual))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaValidationErrorKind {
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    ExpectedObject,
    MissingRequiredField {
        field: String,
    },
    UnexpectedField {
        field: String,
    },
    NullNotAllowed,
    ValueNotAllowed {
        allowed: Vec<Value>,
    },
    TypeMismatch {
        expected: ContextTypeName,
        actual: ContextTypeName,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTypeName {
    Any,
    Null,
    Boolean,
    Integer,
    Number,
    String,
    Array,
    Object,
}

impl fmt::Display for ContextTypeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}",
            match self {
                Self::Any => "any",
                Self::Null => "null",
                Self::Boolean => "boolean",
                Self::Integer => "integer",
                Self::Number => "number",
                Self::String => "string",
                Self::Array => "array",
                Self::Object => "object",
            }
        )
    }
}

impl ContextType {
    pub const fn name(&self) -> ContextTypeName {
        match self {
            Self::Any => ContextTypeName::Any,
            Self::Null => ContextTypeName::Null,
            Self::Boolean => ContextTypeName::Boolean,
            Self::Integer => ContextTypeName::Integer,
            Self::Number => ContextTypeName::Number,
            Self::String { .. } => ContextTypeName::String,
            Self::Array { .. } => ContextTypeName::Array,
            Self::Object { .. } => ContextTypeName::Object,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaValidationError {
    pub path: Vec<ContextPathSegment>,
    pub kind: SchemaValidationErrorKind,
}

impl fmt::Display for SchemaValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = display_context_path(&self.path);
        match &self.kind {
            SchemaValidationErrorKind::UnsupportedVersion { found, supported } => write!(
                formatter,
                "schema at {path} uses version {found}, but supported versions are 1..={supported}"
            ),
            SchemaValidationErrorKind::ExpectedObject => {
                write!(formatter, "context value at {path} must be an object")
            }
            SchemaValidationErrorKind::MissingRequiredField { field } => write!(
                formatter,
                "context value at {path} is missing required field {field}"
            ),
            SchemaValidationErrorKind::UnexpectedField { field } => {
                write!(
                    formatter,
                    "context value at {path} contains unknown field {field}"
                )
            }
            SchemaValidationErrorKind::NullNotAllowed => {
                write!(formatter, "context value at {path} must not be null")
            }
            SchemaValidationErrorKind::ValueNotAllowed { allowed } => write!(
                formatter,
                "context value at {path} must be one of {}",
                Value::Array(allowed.clone())
            ),
            SchemaValidationErrorKind::TypeMismatch { expected, actual } => write!(
                formatter,
                "context value at {path} has type {actual}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for SchemaValidationError {}

fn display_context_path(path: &[ContextPathSegment]) -> String {
    if path.is_empty() {
        return "$".into();
    }
    let mut rendered = String::from("$");
    for segment in path {
        match segment {
            ContextPathSegment::Field { name } => {
                rendered.push('.');
                rendered.push_str(name);
            }
            ContextPathSegment::Index { index } => {
                rendered.push('[');
                rendered.push_str(&index.to_string());
                rendered.push(']');
            }
        }
    }
    rendered
}

fn validate_schema_version(
    version: u32,
    path: &[ContextPathSegment],
) -> Result<(), SchemaValidationError> {
    if version == 0 || version > CONTEXT_SCHEMA_VERSION {
        return Err(SchemaValidationError {
            path: path.to_vec(),
            kind: SchemaValidationErrorKind::UnsupportedVersion {
                found: version,
                supported: CONTEXT_SCHEMA_VERSION,
            },
        });
    }
    Ok(())
}

fn validate_object_value(
    schema: &ObjectSchema,
    value: &Value,
    path: &mut Vec<ContextPathSegment>,
) -> Result<(), SchemaValidationError> {
    validate_schema_version(schema.version, path)?;
    let Some(object) = value.as_object() else {
        return Err(SchemaValidationError {
            path: path.clone(),
            kind: SchemaValidationErrorKind::ExpectedObject,
        });
    };
    for (name, field) in &schema.fields {
        match object.get(name) {
            Some(value) => {
                path.push(ContextPathSegment::field(name));
                let result = validate_field_value(field, value, path);
                path.pop();
                result?;
            }
            None if field.required => {
                return Err(SchemaValidationError {
                    path: path.clone(),
                    kind: SchemaValidationErrorKind::MissingRequiredField {
                        field: name.clone(),
                    },
                });
            }
            None => {}
        }
    }
    for (name, value) in object
        .iter()
        .filter(|(name, _)| !schema.fields.contains_key(*name))
    {
        let Some(value_type) = schema.additional_fields.value_type() else {
            return Err(SchemaValidationError {
                path: path.clone(),
                kind: SchemaValidationErrorKind::UnexpectedField {
                    field: name.clone(),
                },
            });
        };
        path.push(ContextPathSegment::field(name));
        let result = validate_type_value(value_type, value, path);
        path.pop();
        result?;
    }
    Ok(())
}

fn validate_field_value(
    field: &FieldSchema,
    value: &Value,
    path: &mut Vec<ContextPathSegment>,
) -> Result<(), SchemaValidationError> {
    if value.is_null() {
        return if field.nullable || matches!(field.value_type, ContextType::Null | ContextType::Any)
        {
            Ok(())
        } else {
            Err(SchemaValidationError {
                path: path.clone(),
                kind: SchemaValidationErrorKind::NullNotAllowed,
            })
        };
    }
    validate_type_value(&field.value_type, value, path)?;
    if !field.allowed_values.is_empty() && !field.allowed_values.contains(value) {
        return Err(SchemaValidationError {
            path: path.clone(),
            kind: SchemaValidationErrorKind::ValueNotAllowed {
                allowed: field.allowed_values.clone(),
            },
        });
    }
    Ok(())
}

fn validate_type_value(
    expected: &ContextType,
    value: &Value,
    path: &mut Vec<ContextPathSegment>,
) -> Result<(), SchemaValidationError> {
    let valid = match expected {
        ContextType::Any => true,
        ContextType::Null => value.is_null(),
        ContextType::Boolean => value.is_boolean(),
        ContextType::Integer => value
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64()),
        ContextType::Number => value.is_number(),
        ContextType::String { .. } => value.is_string(),
        ContextType::Array { items } => {
            let Some(values) = value.as_array() else {
                return type_mismatch(expected, value, path);
            };
            for (index, item) in values.iter().enumerate() {
                path.push(ContextPathSegment::index(index));
                let result = validate_type_value(items, item, path);
                path.pop();
                result?;
            }
            true
        }
        ContextType::Object { schema } => {
            validate_object_value(schema, value, path)?;
            true
        }
    };
    if valid {
        Ok(())
    } else {
        type_mismatch(expected, value, path)
    }
}

fn type_mismatch<T>(
    expected: &ContextType,
    value: &Value,
    path: &[ContextPathSegment],
) -> Result<T, SchemaValidationError> {
    Err(SchemaValidationError {
        path: path.to_vec(),
        kind: SchemaValidationErrorKind::TypeMismatch {
            expected: expected.name(),
            actual: ContextType::infer(value).name(),
        },
    })
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedSchema<'a> {
    pub value_type: &'a ContextType,
    pub required: bool,
    pub nullable: bool,
    pub sensitivity: Sensitivity,
    pub allowed_values: &'a [Value],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSchemaOwned {
    pub value_type: ContextType,
    pub required: bool,
    pub nullable: bool,
    pub sensitivity: Sensitivity,
    pub allowed_values: Vec<Value>,
}

impl From<ResolvedSchema<'_>> for ResolvedSchemaOwned {
    fn from(resolved: ResolvedSchema<'_>) -> Self {
        Self {
            value_type: resolved.value_type.clone(),
            required: resolved.required,
            nullable: resolved.nullable,
            sensitivity: resolved.sensitivity,
            allowed_values: resolved.allowed_values.to_vec(),
        }
    }
}

fn resolve_field_schema<'a>(
    field: &'a FieldSchema,
    segments: &[ContextPathSegment],
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
) -> Option<ResolvedSchema<'a>> {
    let Some((segment, rest)) = segments.split_first() else {
        return Some(ResolvedSchema {
            value_type: &field.value_type,
            required,
            nullable,
            // A reference to an object/array subtree can expose every value
            // below it. Treat it as sensitive as its most-sensitive child so
            // binding the parent cannot launder a nested secret.
            sensitivity: sensitivity.combine(aggregate_type_sensitivity(&field.value_type)),
            allowed_values: &field.allowed_values,
        });
    };
    match (segment, &field.value_type) {
        (ContextPathSegment::Field { name }, ContextType::Object { schema }) => {
            if let Some(child) = schema.fields.get(name) {
                resolve_field_schema(
                    child,
                    rest,
                    required && child.required,
                    nullable || child.nullable,
                    sensitivity.combine(child.sensitivity),
                )
            } else {
                resolve_type_schema(
                    schema.additional_fields.value_type()?,
                    rest,
                    false,
                    nullable,
                    sensitivity,
                )
            }
        }
        (ContextPathSegment::Index { .. }, ContextType::Array { items }) => {
            // An array's item type is known, but a particular positional index
            // is never guaranteed to exist by the structural schema alone.
            resolve_type_schema(items, rest, false, nullable, sensitivity)
        }
        _ => None,
    }
}

fn resolve_type_schema<'a>(
    value_type: &'a ContextType,
    segments: &[ContextPathSegment],
    required: bool,
    nullable: bool,
    sensitivity: Sensitivity,
) -> Option<ResolvedSchema<'a>> {
    let Some((segment, rest)) = segments.split_first() else {
        return Some(ResolvedSchema {
            value_type,
            required,
            nullable,
            sensitivity: sensitivity.combine(aggregate_type_sensitivity(value_type)),
            allowed_values: &[],
        });
    };
    match (segment, value_type) {
        (ContextPathSegment::Field { name }, ContextType::Object { schema }) => {
            if let Some(child) = schema.fields.get(name) {
                resolve_field_schema(
                    child,
                    rest,
                    required && child.required,
                    nullable || child.nullable,
                    sensitivity.combine(child.sensitivity),
                )
            } else {
                resolve_type_schema(
                    schema.additional_fields.value_type()?,
                    rest,
                    false,
                    nullable,
                    sensitivity,
                )
            }
        }
        (ContextPathSegment::Index { .. }, ContextType::Array { items }) => {
            resolve_type_schema(items, rest, false, nullable, sensitivity)
        }
        _ => None,
    }
}

fn aggregate_object_sensitivity(schema: &ObjectSchema) -> Sensitivity {
    let declared = schema
        .fields
        .values()
        .fold(Sensitivity::Public, |aggregate, field| {
            aggregate.combine(
                field
                    .sensitivity
                    .combine(aggregate_type_sensitivity(&field.value_type)),
            )
        });
    schema
        .additional_fields
        .value_type()
        .map_or(declared, |value_type| {
            declared.combine(aggregate_type_sensitivity(value_type))
        })
}

fn aggregate_type_sensitivity(value_type: &ContextType) -> Sensitivity {
    match value_type {
        ContextType::Array { items } => aggregate_type_sensitivity(items),
        ContextType::Object { schema } => aggregate_object_sensitivity(schema),
        _ => Sensitivity::Public,
    }
}

/// Stable origin scope for a context field. Step and loop scopes use immutable
/// step IDs rather than display names or positional indices.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ContextScope {
    Scenario,
    Step { step_id: String },
    LoopItem { step_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ContextPathSegment {
    Field { name: String },
    Index { index: usize },
}

impl ContextPathSegment {
    pub fn field(name: impl Into<String>) -> Self {
        Self::Field { name: name.into() }
    }

    pub const fn index(index: usize) -> Self {
        Self::Index { index }
    }
}

/// Stable, serialization-safe reference to a value in a context scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldRef {
    pub scope: ContextScope,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<ContextPathSegment>,
}

impl FieldRef {
    pub fn step(step_id: impl Into<String>) -> Self {
        Self {
            scope: ContextScope::Step {
                step_id: step_id.into(),
            },
            segments: Vec::new(),
        }
    }

    pub fn scenario() -> Self {
        Self {
            scope: ContextScope::Scenario,
            segments: Vec::new(),
        }
    }

    pub fn loop_item(step_id: impl Into<String>) -> Self {
        Self {
            scope: ContextScope::LoopItem {
                step_id: step_id.into(),
            },
            segments: Vec::new(),
        }
    }

    pub fn field(mut self, name: impl Into<String>) -> Self {
        self.segments.push(ContextPathSegment::field(name));
        self
    }

    pub fn index(mut self, index: usize) -> Self {
        self.segments.push(ContextPathSegment::index(index));
        self
    }
}

/// Type-neutral input binding. Expected types belong to the consumer's input
/// schema, so bindings remain reusable and serializable without `Binding<T>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Binding {
    Literal {
        value: Value,
    },
    Field {
        field: FieldRef,
    },
    /// Legacy text template retained for schema-v1 compatibility. New graph
    /// files should use `Interpolated`, whose references remain structural
    /// through rename, validation, and serialization.
    Template {
        template: String,
    },
    Interpolated {
        parts: Vec<TemplatePart>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TemplatePart {
    Literal { value: String },
    Field { field: FieldRef },
}

impl Binding {
    pub fn literal(value: impl Into<Value>) -> Self {
        Self::Literal {
            value: value.into(),
        }
    }

    pub fn field(field: FieldRef) -> Self {
        Self::Field { field }
    }

    pub fn template(template: impl Into<String>) -> Self {
        Self::Template {
            template: template.into(),
        }
    }

    pub fn interpolated(parts: impl IntoIterator<Item = TemplatePart>) -> Self {
        Self::Interpolated {
            parts: parts.into_iter().collect(),
        }
    }
}

impl TemplatePart {
    pub fn literal(value: impl Into<String>) -> Self {
        Self::Literal {
            value: value.into(),
        }
    }

    pub fn field(field: FieldRef) -> Self {
        Self::Field { field }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ContextOrigin {
    ScenarioInput,
    Step { step_id: String },
    LoopItem { step_id: String, index: usize },
    Literal,
    Derived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProvenance {
    pub origin: ContextOrigin,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<FieldRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

impl ContextProvenance {
    pub fn step(step_id: impl Into<String>) -> Self {
        Self {
            origin: ContextOrigin::Step {
                step_id: step_id.into(),
            },
            inputs: Vec::new(),
            operation: None,
        }
    }
}

/// One runtime context root and its security/provenance metadata.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextValue {
    #[serde(default = "context_schema_version")]
    pub version: u32,
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<ObjectSchema>,
    /// Declared root type for scalar and array scopes (notably loop items).
    /// Object-producing action outputs keep using `schema` for the compact v1
    /// wire representation; exactly one declaration is normally present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_type: Option<ContextType>,
    pub provenance: ContextProvenance,
    #[serde(default, skip_serializing_if = "is_public")]
    pub sensitivity: Sensitivity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextValueWire {
    #[serde(default = "context_schema_version")]
    version: u32,
    value: Value,
    #[serde(default)]
    schema: Option<ObjectSchema>,
    #[serde(default)]
    root_type: Option<ContextType>,
    provenance: ContextProvenance,
    #[serde(default)]
    sensitivity: Sensitivity,
}

impl<'de> Deserialize<'de> for ContextValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ContextValueWire::deserialize(deserializer)?;
        if wire.schema.is_some() && wire.root_type.is_some() {
            return Err(serde::de::Error::custom(
                "context value must declare either schema or root_type, not both",
            ));
        }
        Ok(Self {
            version: wire.version,
            value: wire.value,
            schema: wire.schema,
            root_type: wire.root_type,
            provenance: wire.provenance,
            sensitivity: wire.sensitivity,
        })
    }
}

impl ContextValue {
    pub fn new(value: Value, provenance: ContextProvenance) -> Self {
        Self {
            version: CONTEXT_SCHEMA_VERSION,
            value,
            schema: None,
            root_type: None,
            provenance,
            sensitivity: Sensitivity::Public,
        }
    }

    pub fn with_schema(mut self, schema: ObjectSchema) -> Self {
        self.schema = Some(schema);
        self.root_type = None;
        self
    }

    pub fn with_type(mut self, value_type: ContextType) -> Self {
        self.root_type = Some(value_type);
        self.schema = None;
        self
    }

    pub fn sensitive(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    /// Return a copy safe for ordinary logs and reports. Values marked secret
    /// either at the context root or in the attached field schema are replaced
    /// without mutating the stored context.
    pub fn redacted_value(&self) -> Value {
        self.redacted_value_at(Sensitivity::Secret)
    }

    /// Redact values at or above `minimum` sensitivity.
    pub fn redacted_value_at(&self, minimum: Sensitivity) -> Value {
        if self.sensitivity >= minimum {
            return Value::String("[REDACTED]".into());
        }
        let mut value = self.value.clone();
        if let Some(value_type) = &self.root_type {
            redact_type_value(&mut value, value_type, Sensitivity::Public, minimum);
        }
        // Programmatic construction can bypass the wire invariant. Apply both
        // declarations so even an invalid state cannot suppress redaction.
        if let Some(schema) = &self.schema {
            redact_object_value(&mut value, schema, Sensitivity::Public, minimum);
        }
        value
    }

    fn has_ambiguous_type_declaration(&self) -> bool {
        self.schema.is_some() && self.root_type.is_some()
    }
}

fn redact_object_value(
    value: &mut Value,
    schema: &ObjectSchema,
    inherited: Sensitivity,
    minimum: Sensitivity,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for (name, field) in &schema.fields {
        let Some(value) = object.get_mut(name) else {
            continue;
        };
        let sensitivity = inherited.combine(field.sensitivity);
        if sensitivity >= minimum {
            *value = Value::String("[REDACTED]".into());
            continue;
        }
        redact_type_value(value, &field.value_type, sensitivity, minimum);
    }
    if let Some(value_type) = schema.additional_fields.value_type() {
        for (name, value) in object {
            if !schema.fields.contains_key(name) {
                redact_type_value(value, value_type, inherited, minimum);
            }
        }
    }
}

fn redact_type_value(
    value: &mut Value,
    value_type: &ContextType,
    inherited: Sensitivity,
    minimum: Sensitivity,
) {
    match value_type {
        ContextType::Object { schema } => {
            redact_object_value(value, schema, inherited, minimum);
        }
        ContextType::Array { items } => {
            if let Some(values) = value.as_array_mut() {
                for value in values {
                    redact_type_value(value, items, inherited, minimum);
                }
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEntry {
    pub scope: ContextScope,
    pub context: ContextValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextStore {
    #[serde(default = "context_schema_version")]
    pub version: u32,
    #[serde(default)]
    entries: Vec<ContextEntry>,
}

impl Default for ContextStore {
    fn default() -> Self {
        Self {
            version: CONTEXT_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolvedContextValue<'a> {
    pub value: &'a Value,
    pub provenance: &'a ContextProvenance,
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextLookupError {
    MissingScope(ContextScope),
    MissingSegment { reference: FieldRef, at: usize },
    InvalidVersion { found: u32, supported: u32 },
    AmbiguousTypeDeclaration { scope: ContextScope },
}

impl fmt::Display for ContextLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingScope(scope) => write!(formatter, "context scope {scope:?} is not available"),
            Self::MissingSegment { reference, at } => write!(
                formatter,
                "context field {reference:?} is missing or has an incompatible value at segment {at}"
            ),
            Self::InvalidVersion { found, supported } => write!(
                formatter,
                "context version {found} is not supported (supported: 1..={supported})"
            ),
            Self::AmbiguousTypeDeclaration { scope } => write!(
                formatter,
                "context scope {scope:?} declares both schema and root_type"
            ),
        }
    }
}

impl std::error::Error for ContextLookupError {}

impl ContextStore {
    pub fn entries(&self) -> &[ContextEntry] {
        &self.entries
    }

    pub fn get(&self, scope: &ContextScope) -> Option<&ContextValue> {
        self.entries
            .iter()
            .find(|entry| &entry.scope == scope)
            .map(|entry| &entry.context)
    }

    pub fn insert(&mut self, scope: ContextScope, context: ContextValue) -> Option<ContextValue> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.scope == scope) {
            return Some(std::mem::replace(&mut entry.context, context));
        }
        self.entries.push(ContextEntry { scope, context });
        None
    }

    pub fn resolve(
        &self,
        reference: &FieldRef,
    ) -> Result<ResolvedContextValue<'_>, ContextLookupError> {
        if self.version == 0 || self.version > CONTEXT_SCHEMA_VERSION {
            return Err(ContextLookupError::InvalidVersion {
                found: self.version,
                supported: CONTEXT_SCHEMA_VERSION,
            });
        }
        let context = self
            .get(&reference.scope)
            .ok_or_else(|| ContextLookupError::MissingScope(reference.scope.clone()))?;
        if context.version == 0 || context.version > CONTEXT_SCHEMA_VERSION {
            return Err(ContextLookupError::InvalidVersion {
                found: context.version,
                supported: CONTEXT_SCHEMA_VERSION,
            });
        }
        if context.has_ambiguous_type_declaration() {
            return Err(ContextLookupError::AmbiguousTypeDeclaration {
                scope: reference.scope.clone(),
            });
        }
        let mut value = &context.value;
        for (index, segment) in reference.segments.iter().enumerate() {
            value = match segment {
                ContextPathSegment::Field { name } => value.get(name),
                ContextPathSegment::Index { index } => value.get(*index),
            }
            .ok_or_else(|| ContextLookupError::MissingSegment {
                reference: reference.clone(),
                at: index,
            })?;
        }
        let sensitivity = context
            .root_type
            .as_ref()
            .and_then(|value_type| {
                resolve_type_schema(
                    value_type,
                    &reference.segments,
                    true,
                    false,
                    Sensitivity::Public,
                )
                .map(|resolved| resolved.sensitivity)
            })
            .or_else(|| {
                context.schema.as_ref().and_then(|schema| {
                    if reference.segments.is_empty() {
                        Some(aggregate_object_sensitivity(schema))
                    } else {
                        schema
                            .resolve(&reference.segments)
                            .map(|resolved| resolved.sensitivity)
                    }
                })
            });
        Ok(ResolvedContextValue {
            value,
            provenance: &context.provenance,
            sensitivity: context.sensitivity.combine(sensitivity.unwrap_or_default()),
        })
    }

    pub fn resolve_schema(&self, reference: &FieldRef) -> Option<ResolvedSchema<'_>> {
        let context = self.get(&reference.scope)?;
        if context.has_ambiguous_type_declaration() {
            return None;
        }
        if let Some(value_type) = &context.root_type {
            return resolve_type_schema(
                value_type,
                &reference.segments,
                true,
                false,
                Sensitivity::Public,
            );
        }
        context.schema.as_ref()?.resolve(&reference.segments)
    }

    /// Resolve an entire context root or a nested field to owned type
    /// information suitable for expression checking.
    pub fn resolve_type_owned(&self, reference: &FieldRef) -> Option<ResolvedSchemaOwned> {
        let context = self.get(&reference.scope)?;
        if context.has_ambiguous_type_declaration() {
            return None;
        }
        if let Some(value_type) = &context.root_type {
            return resolve_type_schema(
                value_type,
                &reference.segments,
                true,
                false,
                Sensitivity::Public,
            )
            .map(ResolvedSchemaOwned::from);
        }
        context.schema.as_ref()?.resolve_owned(&reference.segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repository_schema() -> ObjectSchema {
        ObjectSchema::new("ppduster.github.repository@1")
            .with_field(
                "name",
                FieldSchema::required(ContextType::STRING).described("Repository name"),
            )
            .with_field(
                "url",
                FieldSchema::required(ContextType::string(SemanticFormat::GitUrl)),
            )
            .with_field(
                "default_branch",
                FieldSchema::required(ContextType::string(SemanticFormat::GitRef)).nullable(),
            )
            .with_field(
                "token",
                FieldSchema::optional(ContextType::string(SemanticFormat::SecretRef))
                    .sensitive(Sensitivity::Secret),
            )
    }

    fn github_schema() -> ObjectSchema {
        ObjectSchema::new("ppduster.github.repositories@1")
            .with_field(
                "login",
                FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Internal),
            )
            .with_field(
                "repositories",
                FieldSchema::required(ContextType::array(ContextType::object(repository_schema()))),
            )
    }

    #[test]
    fn schema_and_binding_have_stable_serde_round_trips() {
        let schema = github_schema();
        let encoded = serde_json::to_value(&schema).unwrap();
        assert_eq!(encoded["version"], CONTEXT_SCHEMA_VERSION);
        assert_eq!(encoded["fields"]["repositories"]["type"]["kind"], "array");
        assert_eq!(
            encoded["fields"]["repositories"]["type"]["items"]["schema"]["fields"]["url"]["type"]
                ["format"],
            "git-url"
        );
        assert_eq!(
            serde_json::from_value::<ObjectSchema>(encoded).unwrap(),
            schema
        );

        let binding = Binding::field(
            FieldRef::step("list-repositories")
                .field("repositories")
                .index(3)
                .field("url"),
        );
        let encoded = serde_yaml::to_string(&binding).unwrap();
        let decoded: Binding = serde_yaml::from_str(&encoded).unwrap();
        assert_eq!(decoded, binding);
    }

    #[test]
    fn allowed_values_round_trip_validate_and_participate_in_subtyping() {
        let constrained = FieldSchema::required(ContextType::STRING).with_allowed_values([
            "sh",
            "bash",
            "powershell",
        ]);
        let schema =
            ObjectSchema::new("process-input").with_field("interpreter", constrained.clone());

        schema
            .validate_value(&json!({ "interpreter": "bash" }))
            .unwrap();
        let error = schema
            .validate_value(&json!({ "interpreter": "python" }))
            .unwrap_err();
        assert!(matches!(
            error.kind,
            SchemaValidationErrorKind::ValueNotAllowed { .. }
        ));

        let encoded = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            encoded["fields"]["interpreter"]["allowed_values"],
            json!(["sh", "bash", "powershell"])
        );
        let decoded: ObjectSchema = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, schema);
        assert_eq!(
            decoded
                .resolve_owned(&[ContextPathSegment::field("interpreter")])
                .unwrap()
                .allowed_values,
            json!(["sh", "bash", "powershell"])
                .as_array()
                .unwrap()
                .clone()
        );

        let subset = FieldSchema::required(ContextType::STRING).with_allowed_values(["sh", "bash"]);
        let unrestricted = FieldSchema::required(ContextType::STRING);
        assert!(constrained.accepts(&subset));
        assert!(!subset.accepts(&constrained));
        assert!(!constrained.accepts(&unrestricted));
        assert!(unrestricted.accepts(&constrained));
    }

    #[test]
    fn missing_wire_version_defaults_to_current_version() {
        let schema: ObjectSchema = serde_json::from_value(json!({
            "id": "legacy-schema",
            "fields": {},
            "additional_fields": false
        }))
        .unwrap();
        assert_eq!(schema.version, CONTEXT_SCHEMA_VERSION);

        let context: ContextValue = serde_json::from_value(json!({
            "value": {},
            "provenance": { "origin": { "kind": "scenario-input" } }
        }))
        .unwrap();
        assert_eq!(context.version, CONTEXT_SCHEMA_VERSION);
        assert_eq!(context.sensitivity, Sensitivity::Public);
    }

    #[test]
    fn types_are_structurally_assignable_with_semantic_refinements() {
        assert!(ContextType::Number.is_assignable_from(&ContextType::Integer));
        assert!(!ContextType::Integer.is_assignable_from(&ContextType::Number));
        assert!(ContextType::string(SemanticFormat::Url)
            .is_assignable_from(&ContextType::string(SemanticFormat::GitUrl)));
        assert!(!ContextType::string(SemanticFormat::GitUrl)
            .is_assignable_from(&ContextType::string(SemanticFormat::Url)));
        assert!(ContextType::string(SemanticFormat::Path)
            .is_assignable_from(&ContextType::string(SemanticFormat::DirectoryPath)));

        let expected = ObjectSchema::new("expected")
            .with_field("name", FieldSchema::required(ContextType::STRING))
            .allowing_additional_fields();
        let actual = ObjectSchema::new("actual")
            .with_field("name", FieldSchema::required(ContextType::STRING))
            .with_field("private", FieldSchema::required(ContextType::Boolean));
        assert!(ContextType::object(expected).is_assignable_from(&ContextType::object(actual)));
    }

    #[test]
    fn typed_additional_fields_are_backward_compatible_resolvable_and_validated() {
        let closed: ObjectSchema = serde_json::from_value(json!({
            "id": "closed",
            "additional_fields": false
        }))
        .unwrap();
        assert_eq!(closed.additional_fields, AdditionalProperties::Forbidden);
        assert_eq!(
            serde_json::to_value(&closed).unwrap()["additional_fields"],
            false
        );

        let legacy_open: ObjectSchema = serde_json::from_value(json!({
            "id": "legacy-open",
            "additional_fields": true
        }))
        .unwrap();
        assert_eq!(legacy_open.additional_fields, AdditionalProperties::Any);
        assert_eq!(
            serde_json::to_value(&legacy_open).unwrap()["additional_fields"],
            true
        );

        let strings = ObjectSchema::new("string-map").with_additional_fields(ContextType::STRING);
        let encoded = serde_json::to_value(&strings).unwrap();
        assert_eq!(encoded["additional_fields"]["kind"], "string");
        assert_eq!(
            serde_json::from_value::<ObjectSchema>(encoded).unwrap(),
            strings
        );
        strings
            .validate_value(&json!({ "HOME": "/tmp", "LANG": "ru_RU.UTF-8" }))
            .unwrap();
        for invalid in [json!({ "DEBUG": true }), json!({ "NESTED": {} })] {
            let error = strings.validate_value(&invalid).unwrap_err();
            assert_eq!(
                error.kind,
                SchemaValidationErrorKind::TypeMismatch {
                    expected: ContextTypeName::String,
                    actual: ContextType::infer(
                        invalid.as_object().unwrap().values().next().unwrap()
                    )
                    .name(),
                }
            );
        }
        let resolved = strings
            .resolve(&[ContextPathSegment::field("DYNAMIC_NAME")])
            .unwrap();
        assert_eq!(resolved.value_type, &ContextType::STRING);
        assert!(!resolved.required);
    }

    #[test]
    fn object_subtyping_is_conservative_for_open_and_optional_fields() {
        let closed_consumer = ObjectSchema::new("closed")
            .with_field("name", FieldSchema::required(ContextType::STRING));
        let open_producer = ObjectSchema::new("open")
            .with_field("name", FieldSchema::required(ContextType::STRING))
            .allowing_additional_fields();
        assert!(!closed_consumer.is_assignable_from(&open_producer));

        let optional_string = ObjectSchema::new("optional-string")
            .with_field("value", FieldSchema::optional(ContextType::STRING));
        let optional_boolean = ObjectSchema::new("optional-boolean")
            .with_field("value", FieldSchema::optional(ContextType::Boolean));
        assert!(!optional_string.is_assignable_from(&optional_boolean));

        let string_map =
            ObjectSchema::new("string-map").with_additional_fields(ContextType::STRING);
        assert!(!optional_string.is_assignable_from(&string_map));
        let optional_string_open = optional_string
            .clone()
            .with_additional_fields(ContextType::STRING);
        assert!(optional_string_open.is_assignable_from(&string_map));
        let boolean_map =
            ObjectSchema::new("boolean-map").with_additional_fields(ContextType::Boolean);
        assert!(!optional_string_open.is_assignable_from(&boolean_map));

        let open_string_consumer =
            ObjectSchema::new("string-extras").with_additional_fields(ContextType::STRING);
        let known_boolean_extra = ObjectSchema::new("known-extra")
            .with_field("debug", FieldSchema::optional(ContextType::Boolean));
        assert!(!open_string_consumer.is_assignable_from(&known_boolean_extra));
    }

    #[test]
    fn schema_resolver_tracks_array_shape_nullability_and_sensitivity() {
        let schema = github_schema();
        assert_eq!(
            schema.resolve_owned(&[]).unwrap().sensitivity,
            Sensitivity::Secret,
            "the whole object exposes its nested token"
        );
        let repositories = FieldRef::step("list").field("repositories");
        assert_eq!(
            schema.resolve(&repositories.segments).unwrap().sensitivity,
            Sensitivity::Secret,
            "a public array field must aggregate its item sensitivity"
        );
        let repository = repositories.clone().index(0);
        assert_eq!(
            schema.resolve(&repository.segments).unwrap().sensitivity,
            Sensitivity::Secret,
            "a public item object must aggregate its child sensitivity"
        );
        assert_eq!(
            schema
                .resolve(&repository.clone().field("name").segments)
                .unwrap()
                .sensitivity,
            Sensitivity::Public,
            "a selected public leaf is not tainted by a secret sibling"
        );
        let reference = FieldRef::step("list")
            .field("repositories")
            .index(0)
            .field("token");
        let resolved = schema.resolve(&reference.segments).unwrap();
        assert_eq!(
            resolved.value_type,
            &ContextType::string(SemanticFormat::SecretRef)
        );
        assert!(!resolved.required);
        assert!(!resolved.nullable);
        assert_eq!(resolved.sensitivity, Sensitivity::Secret);

        let branch = FieldRef::step("list")
            .field("repositories")
            .index(0)
            .field("default_branch");
        let resolved = schema.resolve(&branch.segments).unwrap();
        assert!(!resolved.required);
        assert!(resolved.nullable);
    }

    #[test]
    fn schema_validation_covers_required_nullable_types_and_additional_fields() {
        let schema = github_schema();
        schema
            .validate_value(&json!({
                "login": "octocat",
                "repositories": [{
                    "name": "hello-world",
                    "url": "git@github.com:octocat/hello-world.git",
                    "default_branch": null
                }]
            }))
            .unwrap();

        let error = schema
            .validate_value(&json!({ "login": "octocat" }))
            .unwrap_err();
        assert_eq!(
            error.kind,
            SchemaValidationErrorKind::MissingRequiredField {
                field: "repositories".into()
            }
        );

        let error = schema
            .validate_value(&json!({
                "login": "octocat",
                "repositories": [{
                    "name": "hello-world",
                    "url": false,
                    "default_branch": "main"
                }]
            }))
            .unwrap_err();
        assert_eq!(
            error.path,
            vec![
                ContextPathSegment::field("repositories"),
                ContextPathSegment::index(0),
                ContextPathSegment::field("url")
            ]
        );
        assert_eq!(
            error.kind,
            SchemaValidationErrorKind::TypeMismatch {
                expected: ContextTypeName::String,
                actual: ContextTypeName::Boolean
            }
        );

        let error = schema
            .validate_value(&json!({
                "login": "octocat",
                "repositories": [],
                "unknown": true
            }))
            .unwrap_err();
        assert_eq!(
            error.kind,
            SchemaValidationErrorKind::UnexpectedField {
                field: "unknown".into()
            }
        );
    }

    #[test]
    fn explicit_input_targets_use_leaf_nullability_and_preserve_optionality() {
        let nested = ObjectSchema::new("nested-input")
            .with_field("value", FieldSchema::required(ContextType::STRING));
        let schema = ObjectSchema::new("inputs").with_field(
            "options",
            FieldSchema::optional(ContextType::object(nested)).nullable(),
        );
        let path = [
            ContextPathSegment::field("options"),
            ContextPathSegment::field("value"),
        ];

        let readable = schema.resolve_owned(&path).unwrap();
        assert!(!readable.required);
        assert!(readable.nullable);

        let writable = schema.resolve_input_target_owned(&path).unwrap();
        assert!(!writable.required);
        assert!(!writable.nullable);
        assert_eq!(writable.value_type, ContextType::STRING);
    }

    #[test]
    fn context_store_resolves_stable_refs_with_provenance_and_sensitivity() {
        let scope = ContextScope::Step {
            step_id: "list".into(),
        };
        let value = json!({
            "login": "octocat",
            "repositories": [{
                "name": "hello-world",
                "url": "https://github.com/octocat/hello-world.git",
                "default_branch": "main",
                "token": "secret-reference"
            }]
        });
        let context =
            ContextValue::new(value, ContextProvenance::step("list")).with_schema(github_schema());
        let mut store = ContextStore::default();
        assert!(store.insert(scope, context).is_none());

        let reference = FieldRef::step("list")
            .field("repositories")
            .index(0)
            .field("token");
        let resolved = store.resolve(&reference).unwrap();
        assert_eq!(resolved.value, "secret-reference");
        assert_eq!(resolved.sensitivity, Sensitivity::Secret);
        assert_eq!(
            resolved.provenance.origin,
            ContextOrigin::Step {
                step_id: "list".into()
            }
        );
        assert_eq!(
            store.resolve_schema(&reference).unwrap().value_type,
            &ContextType::string(SemanticFormat::SecretRef)
        );
        let root = store.resolve_type_owned(&FieldRef::step("list")).unwrap();
        assert!(matches!(root.value_type, ContextType::Object { .. }));
        assert!(root.required);
    }

    #[test]
    fn redaction_covers_nested_secret_fields_without_mutating_context() {
        let original = json!({
            "login": "octocat",
            "repositories": [{
                "name": "hello-world",
                "url": "https://github.com/octocat/hello-world.git",
                "default_branch": "main",
                "token": "secret-reference"
            }]
        });
        let context = ContextValue::new(original.clone(), ContextProvenance::step("list"))
            .with_schema(github_schema());
        let redacted = context.redacted_value();
        assert_eq!(redacted["login"], "octocat");
        assert_eq!(redacted["repositories"][0]["token"], "[REDACTED]");
        assert_eq!(context.value, original);

        let confidential = context.clone().sensitive(Sensitivity::Confidential);
        assert_eq!(confidential.redacted_value(), context.redacted_value());
        assert_eq!(
            confidential.redacted_value_at(Sensitivity::Confidential),
            "[REDACTED]"
        );
    }

    #[test]
    fn lookup_rejects_missing_segments_and_future_versions() {
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::Scenario,
            ContextValue::new(
                json!({ "name": "scenario" }),
                ContextProvenance {
                    origin: ContextOrigin::ScenarioInput,
                    inputs: Vec::new(),
                    operation: None,
                },
            ),
        );
        let missing = FieldRef::scenario().field("unknown");
        assert!(matches!(
            store.resolve(&missing),
            Err(ContextLookupError::MissingSegment { at: 0, .. })
        ));

        store.version = CONTEXT_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.resolve(&FieldRef::scenario()),
            Err(ContextLookupError::InvalidVersion { .. })
        ));
    }

    #[test]
    fn scalar_and_array_scope_roots_keep_their_declared_types() {
        let mut store = ContextStore::default();
        store.insert(
            ContextScope::LoopItem {
                step_id: "numbers".into(),
            },
            ContextValue::new(json!(7), ContextProvenance::step("numbers"))
                .with_type(ContextType::Integer),
        );
        let reference = FieldRef::loop_item("numbers");
        assert_eq!(store.resolve(&reference).unwrap().value, &json!(7));
        assert_eq!(
            store.resolve_type_owned(&reference).unwrap().value_type,
            ContextType::Integer
        );

        let item_schema = ObjectSchema::new("secret-items").with_field(
            "token",
            FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Secret),
        );
        store.insert(
            ContextScope::LoopItem {
                step_id: "secrets".into(),
            },
            ContextValue::new(
                json!([{ "token": "nested-secret" }]),
                ContextProvenance::step("secrets"),
            )
            .with_type(ContextType::array(ContextType::object(item_schema))),
        );
        let array_root = FieldRef::loop_item("secrets");
        assert_eq!(
            store.resolve(&array_root).unwrap().sensitivity,
            Sensitivity::Secret
        );
        assert_eq!(
            store.resolve(&array_root.index(0)).unwrap().sensitivity,
            Sensitivity::Secret
        );
    }

    #[test]
    fn ambiguous_schema_and_root_type_fail_closed_and_redact_both() {
        let schema = ObjectSchema::new("schema-declaration").with_field(
            "schema_secret",
            FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Secret),
        );
        let root_type = ContextType::object(ObjectSchema::new("type-declaration").with_field(
            "type_secret",
            FieldSchema::required(ContextType::STRING).sensitive(Sensitivity::Secret),
        ));
        let wire = json!({
            "value": {
                "schema_secret": "secret-from-schema",
                "type_secret": "secret-from-type"
            },
            "schema": schema,
            "root_type": root_type,
            "provenance": { "origin": { "kind": "scenario-input" } }
        });
        let error = serde_json::from_value::<ContextValue>(wire.clone()).unwrap_err();
        assert!(error
            .to_string()
            .contains("either schema or root_type, not both"));

        // Public fields allow programmatic construction, so lookup must still
        // reject the invalid state while redaction honors both declarations.
        let invalid = ContextValue {
            version: CONTEXT_SCHEMA_VERSION,
            value: wire["value"].clone(),
            schema: serde_json::from_value(wire["schema"].clone()).ok(),
            root_type: serde_json::from_value(wire["root_type"].clone()).ok(),
            provenance: ContextProvenance {
                origin: ContextOrigin::ScenarioInput,
                inputs: Vec::new(),
                operation: None,
            },
            sensitivity: Sensitivity::Public,
        };
        let redacted = invalid.redacted_value();
        assert_eq!(redacted["schema_secret"], "[REDACTED]");
        assert_eq!(redacted["type_secret"], "[REDACTED]");

        let mut store = ContextStore::default();
        store.insert(ContextScope::Scenario, invalid);
        assert!(matches!(
            store.resolve(&FieldRef::scenario()),
            Err(ContextLookupError::AmbiguousTypeDeclaration { .. })
        ));
        assert!(store
            .resolve_schema(&FieldRef::scenario().field("type_secret"))
            .is_none());
        assert!(store.resolve_type_owned(&FieldRef::scenario()).is_none());
    }
}
