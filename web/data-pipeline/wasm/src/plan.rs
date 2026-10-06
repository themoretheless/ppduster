use super::{
    array_info, format_csv, format_flat, format_json, format_sql, format_template, format_xml,
    matches_filters, matches_value, parse_input, FilterCondition, FilterMode, FilterOperator,
    SourceFormat,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};

const DEFAULT_PREVIEW_LIMIT: usize = 20;
const MAX_PREVIEW_LIMIT: usize = 100;
const MAX_EXPRESSION_DEPTH: usize = 32;
const MAX_EXPRESSION_NODES: usize = 256;
const MAX_FILTER_PATH_SEGMENTS: usize = 64;
const MAX_FILTER_PATH_VALUES: usize = 4096;
const MAX_CACHE_ENTRIES: usize = 128;
const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_KEY_LENGTH: usize = 512;

#[derive(Debug, Deserialize)]
pub(crate) struct ExecutionPlan {
    #[serde(default = "default_plan_version")]
    version: u32,
    #[serde(default = "default_preview_limit")]
    preview_limit: usize,
    steps: Vec<PlanStep>,
}

fn default_plan_version() -> u32 {
    1
}

fn default_preview_limit() -> usize {
    DEFAULT_PREVIEW_LIMIT
}

#[derive(Debug, Deserialize)]
struct PlanInput {
    node_id: String,
    #[serde(default)]
    port: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
enum PlanStep {
    Source {
        node_id: String,
        #[serde(default)]
        cache_key: Option<String>,
        #[serde(default)]
        input: Option<PlanInput>,
        config: SourceConfig,
    },
    Filter {
        node_id: String,
        #[serde(default)]
        cache_key: Option<String>,
        input: Option<PlanInput>,
        config: FilterConfig,
    },
    Project {
        node_id: String,
        #[serde(default)]
        cache_key: Option<String>,
        input: Option<PlanInput>,
        config: ProjectConfig,
    },
    Template {
        node_id: String,
        #[serde(default)]
        cache_key: Option<String>,
        input: Option<PlanInput>,
        config: TemplateConfig,
    },
    Sink {
        node_id: String,
        #[serde(default)]
        cache_key: Option<String>,
        input: Option<PlanInput>,
        config: SinkConfig,
    },
}

#[derive(Debug, Deserialize)]
struct SourceConfig {
    #[serde(alias = "json", alias = "input")]
    data: String,
    #[serde(default)]
    format: SourceFormat,
    #[serde(default)]
    path: String,
    #[serde(default = "default_csv_delimiter")]
    csv_delimiter: String,
}

#[derive(Debug, Deserialize)]
struct FilterConfig {
    #[serde(default)]
    expression: Option<FilterExpression>,
    #[serde(default)]
    filters: Vec<FilterCondition>,
    #[serde(default)]
    filter_mode: FilterMode,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FilterExpression {
    Condition {
        field: String,
        #[serde(default)]
        quantifier: FilterQuantifier,
        operator: FilterOperator,
        #[serde(default)]
        value: String,
    },
    Group {
        operator: FilterGroupOperator,
        children: Vec<FilterExpression>,
    },
    Not {
        child: Box<FilterExpression>,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum FilterGroupOperator {
    And,
    Or,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum FilterQuantifier {
    #[default]
    One,
    Any,
    All,
    None,
}

#[derive(Debug)]
enum FilterPathSegment {
    Root,
    Key(String),
    Index(usize),
    Wildcard,
}

#[derive(Debug, Deserialize)]
struct ProjectConfig {
    fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TemplateConfig {
    #[serde(default = "default_value_template")]
    value_template: String,
    #[serde(default = "super::default_true")]
    strip_outer_quotes: bool,
    #[serde(default = "super::default_true")]
    skip_empty: bool,
}

#[derive(Debug, Deserialize)]
struct SinkConfig {
    #[serde(default)]
    format: SinkFormat,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default = "default_flat_delimiter")]
    delimiter: String,
    #[serde(default = "super::default_true")]
    skip_empty: bool,
    #[serde(default)]
    unique: bool,
    #[serde(default = "default_csv_delimiter")]
    csv_delimiter: String,
    #[serde(default = "super::default_true")]
    csv_include_header: bool,
    #[serde(default)]
    csv_quote_all: bool,
    #[serde(default = "super::default_xml_root")]
    xml_root: String,
    #[serde(default = "super::default_xml_row")]
    xml_row: String,
    #[serde(default = "super::default_table_name")]
    table_name: String,
    #[serde(default = "default_value_template")]
    value_template: String,
    #[serde(default = "super::default_true")]
    strip_outer_quotes: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum SinkFormat {
    #[default]
    Flat,
    Template,
    Json,
    Csv,
    Xml,
    Sql,
}

fn default_csv_delimiter() -> String {
    ",".to_owned()
}

fn default_flat_delimiter() -> String {
    ", ".to_owned()
}

fn default_value_template() -> String {
    "{value}".to_owned()
}

#[derive(Clone)]
struct NodeState {
    data: Option<Vec<Value>>,
    schema: Option<Value>,
}

#[derive(Clone)]
struct CacheEntry {
    outcome: StepOutcome,
    approximate_bytes: usize,
}

#[derive(Default)]
struct PlanCache {
    entries: HashMap<String, CacheEntry>,
    lru: VecDeque<String>,
    approximate_bytes: usize,
}

impl PlanCache {
    fn get(&mut self, key: &str) -> Option<StepOutcome> {
        let entry = self.entries.get(key)?.clone();
        self.lru.retain(|candidate| candidate != key);
        self.lru.push_back(key.to_owned());
        Some(entry.outcome)
    }

    fn insert(&mut self, key: String, outcome: StepOutcome) {
        let approximate_bytes = approximate_outcome_bytes(&outcome) + key.len();
        if approximate_bytes > MAX_CACHE_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.approximate_bytes = self
                .approximate_bytes
                .saturating_sub(previous.approximate_bytes);
            self.lru.retain(|candidate| candidate != &key);
        }
        while self.entries.len() >= MAX_CACHE_ENTRIES
            || self.approximate_bytes + approximate_bytes > MAX_CACHE_BYTES
        {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.approximate_bytes = self
                    .approximate_bytes
                    .saturating_sub(removed.approximate_bytes);
            }
        }
        self.approximate_bytes += approximate_bytes;
        self.lru.push_back(key.clone());
        self.entries.insert(
            key,
            CacheEntry {
                outcome,
                approximate_bytes,
            },
        );
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.approximate_bytes = 0;
    }
}

thread_local! {
    static EXECUTION_CACHE: RefCell<PlanCache> = RefCell::new(PlanCache::default());
}

pub(crate) fn execute(plan: ExecutionPlan) -> Value {
    if plan.version != 1 {
        return json!({
            "ok": false,
            "nodes": {},
            "sink_outputs": {},
            "diagnostics": [diagnostic(
                "unsupported_plan_version",
                &format!("Версия плана {} не поддерживается", plan.version),
                None,
            )],
        });
    }

    let preview_limit = plan.preview_limit.min(MAX_PREVIEW_LIMIT);
    let mut states: HashMap<String, NodeState> = HashMap::new();
    let mut nodes = Map::new();
    let mut sink_outputs = Map::new();
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();

    for step in plan.steps {
        let node_id = step.node_id().to_owned();
        if !seen.insert(node_id.clone()) {
            let issue = diagnostic(
                "duplicate_node_id",
                "Идентификатор узла должен быть уникальным",
                Some(&node_id),
            );
            nodes.insert(node_id.clone(), failure_node(issue.clone(), None));
            diagnostics.push(issue);
            states.insert(
                node_id,
                NodeState {
                    data: None,
                    schema: None,
                },
            );
            continue;
        }

        let outcome = execute_step_cached(&step, &states, preview_limit);
        if let Some(output) = outcome.sink_output {
            sink_outputs.insert(node_id.clone(), Value::String(output));
        }
        if !outcome.ok {
            diagnostics.extend(outcome.diagnostics.iter().cloned());
        }
        states.insert(
            node_id.clone(),
            NodeState {
                data: outcome.data,
                schema: outcome.schema,
            },
        );
        nodes.insert(node_id, outcome.response);
    }

    json!({
        "ok": diagnostics.is_empty(),
        "nodes": nodes,
        "sink_outputs": sink_outputs,
        "diagnostics": diagnostics,
    })
}

#[derive(Clone)]
struct StepOutcome {
    ok: bool,
    data: Option<Vec<Value>>,
    schema: Option<Value>,
    response: Value,
    diagnostics: Vec<Value>,
    sink_output: Option<String>,
}

fn execute_step_cached(
    step: &PlanStep,
    states: &HashMap<String, NodeState>,
    preview_limit: usize,
) -> StepOutcome {
    let cache_identity = step.cache_key().and_then(|cache_key| {
        (cache_key.len() <= MAX_CACHE_KEY_LENGTH)
            .then(|| format!("{}\0{cache_key}\0{preview_limit}", step.node_id()))
    });
    if let Some(identity) = cache_identity.as_deref() {
        if let Some(mut hit) = EXECUTION_CACHE.with(|cache| cache.borrow_mut().get(identity)) {
            set_cached(&mut hit.response, true);
            return hit;
        }
    }

    let mut outcome = execute_step(step, states, preview_limit);
    set_cached(&mut outcome.response, false);
    if outcome.ok {
        if let Some(identity) = cache_identity {
            EXECUTION_CACHE.with(|cache| cache.borrow_mut().insert(identity, outcome.clone()));
        }
    }
    outcome
}

fn set_cached(response: &mut Value, cached: bool) {
    if let Some(object) = response.as_object_mut() {
        object.insert("cached".to_owned(), Value::Bool(cached));
    }
}

fn approximate_outcome_bytes(outcome: &StepOutcome) -> usize {
    serde_json::to_vec(&outcome.response)
        .map(|value| value.len())
        .unwrap_or(0)
        + outcome
            .data
            .as_ref()
            .and_then(|value| serde_json::to_vec(value).ok())
            .map(|value| value.len())
            .unwrap_or(0)
        + outcome.sink_output.as_ref().map(String::len).unwrap_or(0)
}

fn execute_step(
    step: &PlanStep,
    states: &HashMap<String, NodeState>,
    preview_limit: usize,
) -> StepOutcome {
    match step {
        PlanStep::Source {
            node_id,
            input,
            config,
            ..
        } => {
            if input.is_some() {
                return failed(
                    node_id,
                    "source_has_input",
                    "У источника не должно быть входного соединения",
                );
            }
            execute_source(node_id, config, preview_limit)
        }
        PlanStep::Filter {
            node_id,
            input,
            config,
            ..
        } => with_parent(node_id, input, states, |data, schema| {
            execute_filter(node_id, data, schema, config, preview_limit)
        }),
        PlanStep::Project {
            node_id,
            input,
            config,
            ..
        } => with_parent(node_id, input, states, |data, schema| {
            execute_project(node_id, data, schema, config, preview_limit)
        }),
        PlanStep::Template {
            node_id,
            input,
            config,
            ..
        } => with_parent(node_id, input, states, |data, schema| {
            let mut output = Vec::new();
            for value in data {
                let value = value
                    .as_object()
                    .and_then(|object| object.get("value"))
                    .unwrap_or(value);
                let raw = super::value_to_text(value);
                let raw = if config.strip_outer_quotes {
                    super::strip_matching_outer_quotes(&raw)
                } else {
                    raw
                };
                if config.skip_empty && (raw.is_empty() || value.is_null()) {
                    continue;
                }
                output.push(json!({"value": config.value_template.replace("{value}", &raw)}));
            }
            success(
                output.clone(),
                Some(schema.clone()),
                Some(self::schema(&output)),
                preview_limit,
                json!({"input_items":data.len(),"output_items":output.len(),"filtered_out":0,"skipped_items":data.len()-output.len()}),
                None,
            )
        }),
        PlanStep::Sink {
            node_id,
            input,
            config,
            ..
        } => with_parent(node_id, input, states, |data, schema| {
            execute_sink(node_id, data, schema, config, preview_limit)
        }),
    }
}

fn with_parent<F>(
    node_id: &str,
    input: &Option<PlanInput>,
    states: &HashMap<String, NodeState>,
    run: F,
) -> StepOutcome
where
    F: FnOnce(&[Value], &Value) -> StepOutcome,
{
    let Some(input) = input else {
        return failed(
            node_id,
            "missing_input",
            "Узел должен иметь одно входное соединение",
        );
    };
    let _named_port = input.port.as_deref();
    let Some(parent) = states.get(&input.node_id) else {
        return failed(
            node_id,
            "parent_not_ready",
            "Родительский узел отсутствует или расположен позже в плане",
        );
    };
    let Some(data) = parent.data.as_deref() else {
        return failed(
            node_id,
            "upstream_failed",
            "Узел не выполнен из-за ошибки предыдущего шага",
        );
    };
    let Some(schema) = parent.schema.as_ref() else {
        return failed(
            node_id,
            "upstream_schema_missing",
            "Предыдущий шаг не вернул схему данных",
        );
    };
    run(data, schema)
}

fn execute_source(node_id: &str, config: &SourceConfig, preview_limit: usize) -> StepOutcome {
    let parsed = match parse_input(&config.data, config.format, &config.csv_delimiter) {
        Ok(value) => value,
        Err(problem) => {
            return failed(
                node_id,
                "invalid_source",
                &format!(
                    "Некорректные исходные данные: {} ({}:{})",
                    problem.message, problem.line, problem.column
                ),
            )
        }
    };
    let selected = if config.path.is_empty() {
        &parsed
    } else {
        match parsed.pointer(&config.path) {
            Some(value) => value,
            None => {
                return failed(
                    node_id,
                    "path_not_found",
                    "Выбранный путь источника больше не существует",
                )
            }
        }
    };
    let Value::Array(items) = selected else {
        return failed(
            node_id,
            "source_not_records",
            "Источник должен возвращать массив записей",
        );
    };
    let output_schema = schema(items);
    success(
        items.clone(),
        None,
        Some(output_schema),
        preview_limit,
        json!({"input_items": 0, "output_items": items.len(), "filtered_out": 0, "skipped_items": 0}),
        None,
    )
}

fn execute_filter(
    node_id: &str,
    input: &[Value],
    input_schema: &Value,
    config: &FilterConfig,
    preview_limit: usize,
) -> StepOutcome {
    let available = schema_field_set(input_schema);
    if let Some(expression) = &config.expression {
        let mut nodes = 0usize;
        if let Err(problem) =
            validate_expression(expression, &available, "config.expression", 1, &mut nodes)
        {
            return failed_expression(node_id, problem, input_schema.clone());
        }
    } else {
        let missing: Vec<&str> = config
            .filters
            .iter()
            .filter(|condition| !available.contains(condition.field.as_str()))
            .map(|condition| condition.field.as_str())
            .collect();
        if !missing.is_empty() {
            return failed_with_input(
                node_id,
                "field_not_in_input_schema",
                &format!(
                    "Поля условия отсутствуют во входе этого узла: {}",
                    missing.join(", ")
                ),
                Some(input_schema.clone()),
            );
        }
    }

    let mut output = Vec::new();
    let mut filtered_out = 0usize;
    let skipped_items = 0usize;
    for item in input {
        if config.matches(item) {
            output.push(item.clone());
        } else {
            filtered_out += 1;
        }
    }
    let stats = json!({
        "input_items": input.len(),
        "output_items": output.len(),
        "filtered_out": filtered_out,
        "skipped_items": skipped_items,
    });
    success(
        output,
        Some(input_schema.clone()),
        Some(input_schema.clone()),
        preview_limit,
        stats,
        None,
    )
}

struct ExpressionProblem {
    code: &'static str,
    message: String,
    path: String,
}

fn validate_expression(
    expression: &FilterExpression,
    available: &HashSet<&str>,
    path: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ExpressionProblem> {
    if depth > MAX_EXPRESSION_DEPTH {
        return Err(ExpressionProblem {
            code: "expression_too_deep",
            message: format!(
                "Глубина выражения превышает допустимый предел {MAX_EXPRESSION_DEPTH}"
            ),
            path: path.to_owned(),
        });
    }
    *nodes += 1;
    if *nodes > MAX_EXPRESSION_NODES {
        return Err(ExpressionProblem {
            code: "expression_too_large",
            message: format!("Выражение содержит больше {MAX_EXPRESSION_NODES} логических узлов"),
            path: path.to_owned(),
        });
    }

    match expression {
        FilterExpression::Condition { field, .. } => {
            let segments = parse_filter_path(field).map_err(|message| ExpressionProblem {
                code: "invalid_filter_path",
                message,
                path: format!("{path}.field"),
            })?;
            let root_field = segments.iter().find_map(|segment| match segment {
                FilterPathSegment::Root => None,
                FilterPathSegment::Key(key) => Some(key.as_str()),
                FilterPathSegment::Index(_) | FilterPathSegment::Wildcard => None,
            });
            if root_field.is_some_and(|field| !available.contains(field)) {
                return Err(ExpressionProblem {
                    code: "field_not_in_input_schema",
                    message: format!("Поле условия «{field}» отсутствует во входе этого узла"),
                    path: format!("{path}.field"),
                });
            }
        }
        FilterExpression::Group { children, .. } => {
            if children.is_empty() {
                return Err(ExpressionProblem {
                    code: "expression_group_empty",
                    message: "Логическая группа должна содержать хотя бы одно условие".to_owned(),
                    path: format!("{path}.children"),
                });
            }
            for (index, child) in children.iter().enumerate() {
                validate_expression(
                    child,
                    available,
                    &format!("{path}.children[{index}]"),
                    depth + 1,
                    nodes,
                )?;
            }
        }
        FilterExpression::Not { child } => {
            validate_expression(child, available, &format!("{path}.child"), depth + 1, nodes)?;
        }
    }
    Ok(())
}

impl FilterConfig {
    fn matches(&self, item: &Value) -> bool {
        match &self.expression {
            Some(expression) => expression.matches(item),
            None => item
                .as_object()
                .is_some_and(|object| matches_filters(object, &self.filters, self.filter_mode)),
        }
    }
}

impl FilterExpression {
    fn matches(&self, item: &Value) -> bool {
        match self {
            Self::Condition {
                field,
                quantifier,
                operator,
                value,
            } => matches_path_condition(item, field, *quantifier, *operator, value),
            Self::Group { operator, children } => match operator {
                FilterGroupOperator::And => children.iter().all(|child| child.matches(item)),
                FilterGroupOperator::Or => children.iter().any(|child| child.matches(item)),
            },
            Self::Not { child } => !child.matches(item),
        }
    }
}

fn parse_filter_path(path: &str) -> Result<Vec<FilterPathSegment>, String> {
    let chars: Vec<char> = path.chars().collect();
    if chars.is_empty() {
        return Err("Путь условия не может быть пустым".to_owned());
    }

    let mut segments = Vec::new();
    let mut index = 0usize;
    if chars.first() == Some(&'$') {
        segments.push(FilterPathSegment::Root);
        index += 1;
        if index < chars.len() && chars[index] == '.' {
            index += 1;
        }
    }

    while index < chars.len() {
        if segments.len() >= MAX_FILTER_PATH_SEGMENTS {
            return Err(format!(
                "Путь условия содержит больше {MAX_FILTER_PATH_SEGMENTS} сегментов"
            ));
        }

        if chars[index] == '[' {
            let close = chars[index + 1..]
                .iter()
                .position(|character| *character == ']')
                .map(|offset| index + 1 + offset)
                .ok_or_else(|| "В пути условия не закрыта квадратная скобка".to_owned())?;
            let content: String = chars[index + 1..close].iter().collect();
            if content == "*" {
                segments.push(FilterPathSegment::Wildcard);
            } else {
                let array_index = content.parse::<usize>().map_err(|_| {
                    "В квадратных скобках ожидается индекс массива или *".to_owned()
                })?;
                segments.push(FilterPathSegment::Index(array_index));
            }
            index = close + 1;
        } else {
            let start = index;
            while index < chars.len() && chars[index] != '.' && chars[index] != '[' {
                index += 1;
            }
            if start == index {
                return Err("Некорректный пустой сегмент пути условия".to_owned());
            }
            segments.push(FilterPathSegment::Key(chars[start..index].iter().collect()));
        }

        if index < chars.len() && chars[index] == '.' {
            index += 1;
            if index == chars.len() || chars[index] == '.' || chars[index] == '[' {
                return Err("Некорректный пустой сегмент пути условия".to_owned());
            }
        } else if index < chars.len() && chars[index] != '[' {
            return Err("Некорректный путь условия".to_owned());
        }
    }

    if segments.is_empty() {
        return Err("Путь условия не может быть пустым".to_owned());
    }
    Ok(segments)
}

fn resolve_filter_path<'a>(
    root: &'a Value,
    segments: &[FilterPathSegment],
) -> Option<Vec<&'a Value>> {
    let mut current = vec![root];
    for segment in segments {
        let mut next = Vec::new();
        for value in current {
            match segment {
                FilterPathSegment::Root => {
                    if next.len() >= MAX_FILTER_PATH_VALUES {
                        return None;
                    }
                    next.push(value);
                }
                FilterPathSegment::Key(key) => {
                    if let Some(child) = value.as_object().and_then(|object| object.get(key)) {
                        if next.len() >= MAX_FILTER_PATH_VALUES {
                            return None;
                        }
                        next.push(child);
                    }
                }
                FilterPathSegment::Index(index) => {
                    if let Some(child) = value.as_array().and_then(|array| array.get(*index)) {
                        if next.len() >= MAX_FILTER_PATH_VALUES {
                            return None;
                        }
                        next.push(child);
                    }
                }
                FilterPathSegment::Wildcard => match value {
                    Value::Array(items) => {
                        for child in items {
                            if next.len() >= MAX_FILTER_PATH_VALUES {
                                return None;
                            }
                            next.push(child);
                        }
                    }
                    Value::Object(object) => {
                        for child in object.values() {
                            if next.len() >= MAX_FILTER_PATH_VALUES {
                                return None;
                            }
                            next.push(child);
                        }
                    }
                    _ => {}
                },
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    Some(current)
}

fn matches_path_condition(
    item: &Value,
    path: &str,
    quantifier: FilterQuantifier,
    operator: FilterOperator,
    expected: &str,
) -> bool {
    let Ok(segments) = parse_filter_path(path) else {
        return false;
    };
    let Some(values) = resolve_filter_path(item, &segments) else {
        return false;
    };
    match quantifier {
        FilterQuantifier::One => matches_value(values.first().copied(), operator, expected),
        FilterQuantifier::Any => values
            .iter()
            .any(|value| matches_value(Some(*value), operator, expected)),
        FilterQuantifier::All => {
            !values.is_empty()
                && values
                    .iter()
                    .all(|value| matches_value(Some(*value), operator, expected))
        }
        FilterQuantifier::None => values
            .iter()
            .all(|value| !matches_value(Some(*value), operator, expected)),
    }
}

fn execute_project(
    node_id: &str,
    input: &[Value],
    input_schema: &Value,
    config: &ProjectConfig,
    preview_limit: usize,
) -> StepOutcome {
    let available = schema_field_set(input_schema);
    let missing: Vec<&str> = config
        .fields
        .iter()
        .filter(|field| !available.contains(field.as_str()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return failed_with_input(
            node_id,
            "field_not_in_input_schema",
            &format!("Поля проекции отсутствуют во входе: {}", missing.join(", ")),
            Some(input_schema.clone()),
        );
    }

    let mut output = Vec::new();
    let mut skipped_items = 0usize;
    for item in input {
        let Value::Object(object) = item else {
            skipped_items += 1;
            continue;
        };
        let mut projected = Map::new();
        for field in &config.fields {
            projected.insert(
                field.clone(),
                object.get(field).cloned().unwrap_or(Value::Null),
            );
        }
        output.push(Value::Object(projected));
    }
    let stats = json!({
        "input_items": input.len(),
        "output_items": output.len(),
        "filtered_out": 0,
        "skipped_items": skipped_items,
    });
    let output_schema = project_schema(input_schema, &config.fields);
    success(
        output,
        Some(input_schema.clone()),
        Some(output_schema),
        preview_limit,
        stats,
        None,
    )
}

fn execute_sink(
    node_id: &str,
    input: &[Value],
    input_schema: &Value,
    config: &SinkConfig,
    preview_limit: usize,
) -> StepOutcome {
    let available = schema_field_set(input_schema);
    let fields: Vec<String> = if config.fields.is_empty() {
        schema_field_names(input_schema)
    } else {
        config.fields.clone()
    };
    let missing: Vec<&str> = fields
        .iter()
        .filter(|field| !available.contains(field.as_str()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return failed_with_input(
            node_id,
            "field_not_in_input_schema",
            &format!("Поля вывода отсутствуют во входе: {}", missing.join(", ")),
            Some(input_schema.clone()),
        );
    }

    let objects: Vec<&Map<String, Value>> = input.iter().filter_map(Value::as_object).collect();
    let skipped_items = input.len().saturating_sub(objects.len());
    let mut empty_values = 0usize;
    let (output, values) = match config.format {
        SinkFormat::Flat => format_flat(
            &objects,
            &fields,
            &config.delimiter,
            config.skip_empty,
            config.unique,
            &mut empty_values,
        ),
        SinkFormat::Template => format_template(
            &objects,
            &fields,
            &config.delimiter,
            &config.value_template,
            config.strip_outer_quotes,
            config.skip_empty,
            config.unique,
            &mut empty_values,
        ),
        SinkFormat::Json => format_json(&objects, &fields, &mut empty_values),
        SinkFormat::Csv => format_csv(
            &objects,
            &fields,
            &config.csv_delimiter,
            config.csv_include_header,
            config.csv_quote_all,
            &mut empty_values,
        ),
        SinkFormat::Xml => format_xml(
            &objects,
            &fields,
            &config.xml_root,
            &config.xml_row,
            &mut empty_values,
        ),
        SinkFormat::Sql => format_sql(&objects, &fields, &config.table_name, &mut empty_values),
    };
    let stats = json!({
        "input_items": input.len(),
        "output_items": objects.len(),
        "filtered_out": 0,
        "skipped_items": skipped_items,
        "empty_values": empty_values,
        "values": values,
    });
    success(
        input.to_vec(),
        Some(input_schema.clone()),
        None,
        preview_limit,
        stats,
        Some(output),
    )
}

fn success(
    data: Vec<Value>,
    input_schema: Option<Value>,
    output_schema: Option<Value>,
    preview_limit: usize,
    stats: Value,
    sink_output: Option<String>,
) -> StepOutcome {
    let preview: Vec<Value> = data.iter().take(preview_limit).cloned().collect();
    let execution_schema = output_schema
        .clone()
        .or_else(|| input_schema.clone())
        .unwrap_or_else(|| json!({"kind": "records", "fields": []}));
    let response = json!({
        "ok": true,
        "preview": preview,
        "preview_truncated": data.len() > preview_limit,
        "input_schema": input_schema,
        "output_schema": output_schema,
        "schema": execution_schema,
        "stats": stats,
        "diagnostics": [],
    });
    StepOutcome {
        ok: true,
        data: Some(data),
        schema: Some(execution_schema),
        response,
        diagnostics: Vec::new(),
        sink_output,
    }
}

fn failed(node_id: &str, code: &str, message: &str) -> StepOutcome {
    failed_with_input(node_id, code, message, None)
}

fn failed_with_input(
    node_id: &str,
    code: &str,
    message: &str,
    input_schema: Option<Value>,
) -> StepOutcome {
    let issue = diagnostic(code, message, Some(node_id));
    StepOutcome {
        ok: false,
        data: None,
        schema: None,
        response: failure_node(issue.clone(), input_schema),
        diagnostics: vec![issue],
        sink_output: None,
    }
}

fn failed_expression(
    node_id: &str,
    problem: ExpressionProblem,
    input_schema: Value,
) -> StepOutcome {
    let issue = json!({
        "severity": "error",
        "code": problem.code,
        "message": problem.message,
        "node_id": node_id,
        "expression_path": problem.path,
    });
    StepOutcome {
        ok: false,
        data: None,
        schema: None,
        response: failure_node(issue.clone(), Some(input_schema)),
        diagnostics: vec![issue],
        sink_output: None,
    }
}

fn failure_node(issue: Value, input_schema: Option<Value>) -> Value {
    json!({
        "ok": false,
        "cached": false,
        "preview": [],
        "preview_truncated": false,
        "input_schema": input_schema,
        "output_schema": null,
        "schema": null,
        "stats": {"input_items": 0, "output_items": 0, "filtered_out": 0, "skipped_items": 0},
        "diagnostics": [issue],
    })
}

fn diagnostic(code: &str, message: &str, node_id: Option<&str>) -> Value {
    json!({
        "severity": "error",
        "code": code,
        "message": message,
        "node_id": node_id,
    })
}

fn schema_field_set(input_schema: &Value) -> HashSet<&str> {
    input_schema
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|field| field.get("name").and_then(Value::as_str))
        .collect()
}

fn schema_field_names(input_schema: &Value) -> Vec<String> {
    input_schema
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|field| field.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn project_schema(input_schema: &Value, selected: &[String]) -> Value {
    let available: HashMap<&str, &Value> = input_schema
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|field| {
            field
                .get("name")
                .and_then(Value::as_str)
                .map(|name| (name, field))
        })
        .collect();
    let fields: Vec<Value> = selected
        .iter()
        .filter_map(|name| available.get(name.as_str()).copied().cloned())
        .collect();
    json!({"kind": "records", "fields": fields})
}

fn schema(items: &[Value]) -> Value {
    let info = array_info(items, "", "");
    json!({
        "kind": "records",
        "fields": info.get("fields").cloned().unwrap_or_else(|| json!([])),
    })
}

impl PlanStep {
    fn node_id(&self) -> &str {
        match self {
            Self::Source { node_id, .. }
            | Self::Filter { node_id, .. }
            | Self::Project { node_id, .. }
            | Self::Template { node_id, .. }
            | Self::Sink { node_id, .. } => node_id,
        }
    }

    fn cache_key(&self) -> Option<&str> {
        match self {
            Self::Source { cache_key, .. }
            | Self::Filter { cache_key, .. }
            | Self::Project { cache_key, .. }
            | Self::Template { cache_key, .. }
            | Self::Sink { cache_key, .. } => cache_key.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_cache() {
        EXECUTION_CACHE.with(|cache| cache.borrow_mut().clear());
    }

    fn run(value: Value) -> Value {
        let plan: ExecutionPlan = serde_json::from_value(value).unwrap();
        execute(plan)
    }

    fn source() -> Value {
        let data = json!({"stores": [
            {"id":"A1","name":"Москва","state":1},
            {"id":"B2","name":"Белгород","state":0}
        ]})
        .to_string();
        json!({
            "node_id": "source",
            "node_type": "source",
            "config": {
                "data": data,
                "format": "json",
                "path": "/stores"
            }
        })
    }

    fn cached(mut step: Value, key: &str) -> Value {
        step.as_object_mut()
            .unwrap()
            .insert("cache_key".to_owned(), Value::String(key.to_owned()));
        step
    }

    fn field_names(schema: &Value) -> Vec<&str> {
        schema["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn project_then_filtering_removed_field_is_an_error() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"project","node_type":"project","input":{"node_id":"source","port":"records"},"config":{"fields":["id","name"]}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"project","port":"matched"},"config":{"filters":[{"field":"state","operator":"equal","value":"1"}],"filter_mode":"all"}}
            ]
        }));
        assert_eq!(result["ok"], false);
        assert_eq!(result["nodes"]["source"]["input_schema"], Value::Null);
        assert_eq!(
            field_names(&result["nodes"]["project"]["output_schema"]),
            vec!["id", "name"]
        );
        assert_eq!(
            result["nodes"]["filter"]["input_schema"],
            result["nodes"]["project"]["output_schema"]
        );
        assert_eq!(result["nodes"]["filter"]["ok"], false);
        assert_eq!(
            result["nodes"]["filter"]["diagnostics"][0]["code"],
            "field_not_in_input_schema"
        );
    }

    #[test]
    fn source_to_filter_reports_explicit_stable_schemas() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"filters":[{"field":"state","operator":"equal","value":"1"}]}}
            ]
        }));
        let expected = json!({
            "kind": "records",
            "fields": [
                {"name":"id", "kind":"string", "present":2},
                {"name":"name", "kind":"string", "present":2},
                {"name":"state", "kind":"number", "present":2}
            ]
        });
        assert_eq!(result["nodes"]["source"]["input_schema"], Value::Null);
        assert_eq!(result["nodes"]["source"]["output_schema"], expected);
        assert_eq!(result["nodes"]["filter"]["input_schema"], expected);
        assert_eq!(result["nodes"]["filter"]["output_schema"], expected);
    }

    #[test]
    fn evaluates_nested_and_or_not_expression() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression": {
                        "kind":"group", "operator":"and", "children":[
                            {"kind":"condition","field":"state","operator":"equal","value":"1"},
                            {"kind":"group","operator":"or","children":[
                                {"kind":"condition","field":"name","operator":"contains","value":"Моск"},
                                {"kind":"not","child":{"kind":"condition","field":"id","operator":"equal","value":"B2"}}
                            ]}
                        ]
                    },
                    "filters":[{"field":"state","operator":"equal","value":"0"}],
                    "filter_mode":"all"
                }}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(result["nodes"]["filter"]["stats"]["output_items"], 1);
        assert_eq!(result["nodes"]["filter"]["preview"][0]["id"], "A1");
    }

    #[test]
    fn filters_nested_objects_and_array_values_with_quantifiers() {
        let data = json!({"users": [
            {"id":"A1", "profile":{"age":25}, "tags":["vip", "new"]},
            {"id":"B2", "profile":{"age":17}, "tags":["new"]},
            {"id":"C3", "profile":{"age":31}, "tags":["blocked", "vip"]}
        ]})
        .to_string();
        let result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":data,"format":"json","path":"/users"}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"group","operator":"and","children":[
                        {"kind":"condition","field":"profile.age","operator":"greater_or_equal","value":"18"},
                        {"kind":"condition","field":"tags[*]","quantifier":"any","operator":"equal","value":"vip"},
                        {"kind":"condition","field":"tags[*]","quantifier":"none","operator":"equal","value":"blocked"}
                    ]}
                }}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(result["nodes"]["filter"]["stats"]["output_items"], 1);
        assert_eq!(result["nodes"]["filter"]["preview"][0]["id"], "A1");
    }

    #[test]
    fn root_path_filters_scalar_and_array_items() {
        let data = json!({"items": [7, 12, ["vip", "new"], ["new"]]}).to_string();
        let scalar_result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":data,"format":"json","path":"/items"}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"condition","field":"$","operator":"greater_than","value":"10"}
                }}
            ]
        }));
        assert_eq!(scalar_result["nodes"]["filter"]["preview"], json!([12]));

        let array_result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":data,"format":"json","path":"/items"}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"condition","field":"$[*]","quantifier":"any","operator":"equal","value":"vip"}
                }}
            ]
        }));
        assert_eq!(
            array_result["nodes"]["filter"]["preview"],
            json!([["vip", "new"]])
        );
    }

    #[test]
    fn rejects_malformed_filter_path() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"condition","field":"name[","operator":"exists"}
                }}
            ]
        }));
        assert_eq!(
            result["nodes"]["filter"]["diagnostics"][0]["code"],
            "invalid_filter_path"
        );
    }

    #[test]
    fn oversized_wildcard_match_fails_closed() {
        let values = vec![json!(0); MAX_FILTER_PATH_VALUES + 1];
        let data = json!({"items": [{"values": values}]}).to_string();
        let result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":data,"format":"json","path":"/items"}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"condition","field":"values[*]","quantifier":"none","operator":"equal","value":"1"}
                }}
            ]
        }));
        assert_eq!(result["nodes"]["filter"]["stats"]["output_items"], 0);
    }

    #[test]
    fn legacy_filters_and_mode_remain_supported() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "filters":[
                        {"field":"state","operator":"equal","value":"0"},
                        {"field":"name","operator":"contains","value":"Моск"}
                    ],
                    "filter_mode":"any"
                }}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(result["nodes"]["filter"]["stats"]["output_items"], 2);
    }

    #[test]
    fn nested_field_error_reports_expression_path() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{
                    "expression":{"kind":"group","operator":"and","children":[
                        {"kind":"condition","field":"state","operator":"equal","value":"1"},
                        {"kind":"not","child":{"kind":"condition","field":"removed","operator":"exists"}}
                    ]}
                }}
            ]
        }));
        let issue = &result["nodes"]["filter"]["diagnostics"][0];
        assert_eq!(issue["code"], "field_not_in_input_schema");
        assert_eq!(
            issue["expression_path"],
            "config.expression.children[1].child.field"
        );
    }

    #[test]
    fn rejects_expression_over_depth_and_size_limits() {
        let mut deep = json!({"kind":"condition","field":"id","operator":"exists"});
        for _ in 0..MAX_EXPRESSION_DEPTH {
            deep = json!({"kind":"not","child":deep});
        }
        let deep_result = run(json!({
            "version":1,
            "steps":[source(),{"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"expression":deep}}]
        }));
        assert_eq!(
            deep_result["nodes"]["filter"]["diagnostics"][0]["code"],
            "expression_too_deep"
        );

        let children: Vec<Value> = (0..=MAX_EXPRESSION_NODES)
            .map(|_| json!({"kind":"condition","field":"id","operator":"exists"}))
            .collect();
        let large_result = run(json!({
            "version":1,
            "steps":[source(),{"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"expression":{"kind":"group","operator":"or","children":children}}}]
        }));
        assert_eq!(
            large_result["nodes"]["filter"]["diagnostics"][0]["code"],
            "expression_too_large"
        );
    }

    #[test]
    fn filter_then_project_uses_actual_parent_result() {
        let result = run(json!({
            "version": 1,
            "preview_limit": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source","port":"records"},"config":{"filters":[{"field":"state","operator":"equal","value":"1"}],"filter_mode":"all"}},
                {"node_id":"project","node_type":"project","input":{"node_id":"filter","port":"matched"},"config":{"fields":["id","name"]}},
                {"node_id":"sink","node_type":"sink","input":{"node_id":"project","port":"matched"},"config":{"format":"csv","csv_delimiter":";"}}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["nodes"]["filter"]["input_schema"],
            result["nodes"]["source"]["output_schema"]
        );
        assert_eq!(
            result["nodes"]["filter"]["output_schema"],
            result["nodes"]["filter"]["input_schema"]
        );
        assert_eq!(
            result["nodes"]["project"]["input_schema"],
            result["nodes"]["filter"]["output_schema"]
        );
        assert_eq!(
            field_names(&result["nodes"]["project"]["output_schema"]),
            vec!["id", "name"]
        );
        assert_eq!(
            result["nodes"]["project"]["output_schema"]["fields"][0]["kind"],
            "string"
        );
        assert_eq!(result["nodes"]["filter"]["stats"]["output_items"], 1);
        assert_eq!(
            result["nodes"]["project"]["schema"]["fields"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(result["nodes"]["project"]["preview"][0]["id"], "A1");
        assert_eq!(result["sink_outputs"]["sink"], "id;name\nA1;Москва");
    }

    #[test]
    fn executes_csv_source_and_bounds_preview() {
        let result = run(json!({
            "version": 1,
            "preview_limit": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":"id,name,state\nA1,Москва,1\nB2,Белгород,0","format":"csv","csv_delimiter":","}},
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"filters":[],"filter_mode":"any"}},
                {"node_id":"sink","node_type":"sink","input":{"node_id":"filter"},"config":{"format":"json","fields":["id","name"]}}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["nodes"]["source"]["preview"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(result["nodes"]["source"]["preview_truncated"], true);
        assert!(result["sink_outputs"]["sink"]
            .as_str()
            .unwrap()
            .contains("Белгород"));
    }

    #[test]
    fn formats_each_selected_value_with_a_template() {
        let quoted_data = json!([
            {"id":"\"000D3A23B0DC80D811E68F98E099F565\""},
            {"id":"\"000D3A22FA54A81611EB145B4F1E2FEB\""}
        ])
        .to_string();
        let result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":quoted_data,"format":"json","path":""}},
                {"node_id":"sink","node_type":"sink","input":{"node_id":"source"},"config":{"format":"template","fields":["id"],"delimiter":",\n","value_template":"0x{value}","strip_outer_quotes":true}}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["sink_outputs"]["sink"],
            "0x000D3A23B0DC80D811E68F98E099F565,\n0x000D3A22FA54A81611EB145B4F1E2FEB"
        );
    }

    #[test]
    fn transforms_a_plain_quoted_list_without_json_wrapping() {
        let list = "\"000D3A23B0DC80D811E68F98E099F565\",\n\"000D3A22FA54A81611EB145B4F1E2FEB\",\n\"000D3A21DA51A81211E9DC4536C21386\"";
        let result = run(json!({
            "version": 1,
            "steps": [
                {"node_id":"source","node_type":"source","config":{"data":list,"format":"list","path":""}},
                {"node_id":"sink","node_type":"sink","input":{"node_id":"source"},"config":{"format":"template","fields":["value"],"delimiter":",\n","value_template":"0x{value}"}}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["sink_outputs"]["sink"],
            "0x000D3A23B0DC80D811E68F98E099F565,\n0x000D3A22FA54A81611EB145B4F1E2FEB,\n0x000D3A21DA51A81211E9DC4536C21386"
        );
    }

    #[test]
    fn preserves_schema_after_filter_returns_no_rows() {
        let result = run(json!({
            "version": 1,
            "steps": [
                source(),
                {"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"filters":[{"field":"state","operator":"equal","value":"99"}]}},
                {"node_id":"project","node_type":"project","input":{"node_id":"filter"},"config":{"fields":["id","name"]}}
            ]
        }));
        assert_eq!(result["ok"], true);
        assert_eq!(result["nodes"]["project"]["preview"], json!([]));
        assert_eq!(
            result["nodes"]["filter"]["output_schema"],
            result["nodes"]["filter"]["input_schema"]
        );
        assert_eq!(
            field_names(&result["nodes"]["filter"]["output_schema"]),
            vec!["id", "name", "state"]
        );
        assert_eq!(
            result["nodes"]["project"]["input_schema"],
            result["nodes"]["filter"]["output_schema"]
        );
        assert_eq!(
            result["nodes"]["project"]["schema"]["fields"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn execute_plan_is_available_through_legacy_entrypoint() {
        let request = json!({
            "action": "execute_plan",
            "plan": {"version": 1, "steps": [source()]}
        });
        let response: Value =
            serde_json::from_str(&crate::process_request(&request.to_string())).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["nodes"]["source"]["stats"]["output_items"], 2);
    }

    #[test]
    fn repeated_plan_returns_all_cache_hits() {
        clear_cache();
        let build = || {
            json!({
                "version":1,
                "steps":[
                    cached(source(), "repeat-source"),
                    cached(json!({"node_id":"filter","node_type":"filter","input":{"node_id":"source"},"config":{"filters":[{"field":"state","operator":"equal","value":"1"}]}}), "repeat-filter"),
                    cached(json!({"node_id":"sink","node_type":"sink","input":{"node_id":"filter"},"config":{"format":"json"}}), "repeat-sink")
                ]
            })
        };
        let first = run(build());
        let second = run(build());
        for node_id in ["source", "filter", "sink"] {
            assert_eq!(first["nodes"][node_id]["cached"], false);
            assert_eq!(second["nodes"][node_id]["cached"], true);
        }
        assert_eq!(
            first["sink_outputs"]["sink"],
            second["sink_outputs"]["sink"]
        );
    }

    #[test]
    fn changed_middle_key_invalidates_descendant_but_not_sibling() {
        clear_cache();
        let build = |project_a_key: &str, sink_a_key: &str| {
            json!({
                "version":1,
                "steps":[
                    cached(source(), "branch-source"),
                    cached(json!({"node_id":"project-a","node_type":"project","input":{"node_id":"source"},"config":{"fields":["id"]}}), project_a_key),
                    cached(json!({"node_id":"sink-a","node_type":"sink","input":{"node_id":"project-a"},"config":{"format":"json"}}), sink_a_key),
                    cached(json!({"node_id":"project-b","node_type":"project","input":{"node_id":"source"},"config":{"fields":["name"]}}), "branch-project-b"),
                    cached(json!({"node_id":"sink-b","node_type":"sink","input":{"node_id":"project-b"},"config":{"format":"json"}}), "branch-sink-b")
                ]
            })
        };
        let first = run(build("branch-project-a-v1", "branch-sink-a-v1"));
        assert_eq!(first["ok"], true);
        let second = run(build("branch-project-a-v2", "branch-sink-a-v2"));
        assert_eq!(second["nodes"]["source"]["cached"], true);
        assert_eq!(second["nodes"]["project-a"]["cached"], false);
        assert_eq!(second["nodes"]["sink-a"]["cached"], false);
        assert_eq!(second["nodes"]["project-b"]["cached"], true);
        assert_eq!(second["nodes"]["sink-b"]["cached"], true);
    }

    #[test]
    fn failed_node_is_never_cached() {
        clear_cache();
        let build = || {
            json!({
                "version":1,
                "steps":[
                    cached(source(), "error-source"),
                    cached(json!({"node_id":"bad-project","node_type":"project","input":{"node_id":"source"},"config":{"fields":["removed"]}}), "same-error-key")
                ]
            })
        };
        let first = run(build());
        let second = run(build());
        assert_eq!(first["nodes"]["bad-project"]["ok"], false);
        assert_eq!(first["nodes"]["bad-project"]["cached"], false);
        assert_eq!(second["nodes"]["bad-project"]["cached"], false);
        assert_eq!(second["nodes"]["source"]["cached"], true);
    }

    #[test]
    fn cache_evicts_lru_entries_and_skips_oversize_values() {
        clear_cache();
        for index in 0..=MAX_CACHE_ENTRIES {
            let result = run(json!({
                "version":1,
                "preview_limit":0,
                "steps":[cached(source(), &format!("eviction-{index}"))]
            }));
            assert_eq!(result["nodes"]["source"]["cached"], false);
        }
        let evicted = run(json!({
            "version":1,
            "preview_limit":0,
            "steps":[cached(source(), "eviction-0")]
        }));
        assert_eq!(evicted["nodes"]["source"]["cached"], false);

        clear_cache();
        let large_data = json!([{"payload":"x".repeat(MAX_CACHE_BYTES + 1)}]).to_string();
        let build_large = || {
            json!({
                "version":1,
                "preview_limit":0,
                "steps":[{
                    "node_id":"large-source",
                    "node_type":"source",
                    "cache_key":"oversize",
                    "config":{"data":large_data,"format":"json"}
                }]
            })
        };
        assert_eq!(run(build_large())["nodes"]["large-source"]["cached"], false);
        assert_eq!(run(build_large())["nodes"]["large-source"]["cached"], false);
    }
}
