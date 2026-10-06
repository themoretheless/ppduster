use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::HashSet;
use themoretheless_tokenizer::{tokenize_json, TokenKind};
use wasm_bindgen::prelude::*;

mod plan;

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum FilterMode {
    #[default]
    All,
    Any,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum SourceFormat {
    #[default]
    Json,
    Csv,
    List,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum OutputFormat {
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

fn default_table_name() -> String {
    "result".to_owned()
}

fn default_xml_root() -> String {
    "rows".to_owned()
}

fn default_xml_row() -> String {
    "row".to_owned()
}

fn default_true() -> bool {
    true
}

fn default_value_template() -> String {
    "{value}".to_owned()
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum FilterOperator {
    Equal,
    NotEqual,
    GreaterThan,
    GreaterOrEqual,
    LessThan,
    LessOrEqual,
    Contains,
    StartsWith,
    EndsWith,
    Exists,
    NotExists,
}

#[derive(Debug, Deserialize)]
struct FilterCondition {
    field: String,
    operator: FilterOperator,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum EngineRequest {
    TokenizeJson {
        source: String,
    },
    ExecutePlan {
        plan: plan::ExecutionPlan,
    },
    Analyze {
        json: String,
        #[serde(default)]
        source_format: SourceFormat,
        #[serde(default = "default_csv_delimiter")]
        csv_delimiter: String,
    },
    FilterPreview {
        json: String,
        path: String,
        #[serde(default)]
        filters: Vec<FilterCondition>,
        #[serde(default)]
        filter_mode: FilterMode,
        #[serde(default)]
        source_format: SourceFormat,
        #[serde(default = "default_csv_delimiter")]
        csv_delimiter: String,
    },
    Transform {
        json: String,
        path: String,
        fields: Vec<String>,
        delimiter: String,
        skip_empty: bool,
        unique: bool,
        #[serde(default)]
        filters: Vec<FilterCondition>,
        #[serde(default)]
        filter_mode: FilterMode,
        #[serde(default)]
        source_format: SourceFormat,
        #[serde(default = "default_csv_delimiter")]
        csv_delimiter: String,
        #[serde(default)]
        output_format: OutputFormat,
        #[serde(default = "default_csv_delimiter")]
        output_csv_delimiter: String,
        #[serde(default = "default_true")]
        csv_include_header: bool,
        #[serde(default)]
        csv_quote_all: bool,
        #[serde(default = "default_xml_root")]
        xml_root: String,
        #[serde(default = "default_xml_row")]
        xml_row: String,
        #[serde(default = "default_table_name")]
        table_name: String,
        #[serde(default = "default_value_template")]
        value_template: String,
        #[serde(default = "default_true")]
        strip_outer_quotes: bool,
    },
}

#[derive(Debug)]
struct ParseProblem {
    message: String,
    line: usize,
    column: usize,
}

#[wasm_bindgen]
pub fn engine_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[wasm_bindgen]
pub fn process_request(request_json: &str) -> String {
    let response = match serde_json::from_str::<EngineRequest>(request_json) {
        Ok(request) => handle_request(request),
        Err(error) => error_response(
            "Не удалось прочитать параметры операции",
            error.line(),
            error.column(),
        ),
    };

    serde_json::to_string(&response).unwrap_or_else(|_| {
        r#"{"ok":false,"error":{"message":"Внутренняя ошибка сериализации","line":0,"column":0}}"#.to_owned()
    })
}

fn handle_request(request: EngineRequest) -> Value {
    match request {
        EngineRequest::TokenizeJson { source } => tokenize_json_response(&source),
        EngineRequest::ExecutePlan { plan } => plan::execute(plan),
        EngineRequest::Analyze {
            json,
            source_format,
            csv_delimiter,
        } => analyze(&json, source_format, &csv_delimiter),
        EngineRequest::FilterPreview {
            json,
            path,
            filters,
            filter_mode,
            source_format,
            csv_delimiter,
        } => filter_preview(
            &json,
            &path,
            &filters,
            filter_mode,
            source_format,
            &csv_delimiter,
        ),
        EngineRequest::Transform {
            json,
            path,
            fields,
            delimiter,
            skip_empty,
            unique,
            filters,
            filter_mode,
            source_format,
            csv_delimiter,
            output_format,
            output_csv_delimiter,
            csv_include_header,
            csv_quote_all,
            xml_root,
            xml_row,
            table_name,
            value_template,
            strip_outer_quotes,
        } => transform(
            &json,
            &path,
            &fields,
            &delimiter,
            skip_empty,
            unique,
            &filters,
            filter_mode,
            source_format,
            &csv_delimiter,
            output_format,
            &output_csv_delimiter,
            csv_include_header,
            csv_quote_all,
            &xml_root,
            &xml_row,
            &table_name,
            &value_template,
            strip_outer_quotes,
        ),
    }
}

fn tokenize_json_response(source: &str) -> Value {
    let tokenization = tokenize_json(source);
    let mut byte_cursor = 0usize;
    let mut utf16_cursor = 0usize;
    let mut byte_to_utf16 = |target: usize| {
        if target > byte_cursor {
            utf16_cursor += source[byte_cursor..target].encode_utf16().count();
            byte_cursor = target;
        }
        utf16_cursor
    };

    let tokens = tokenization
        .tokens
        .iter()
        .map(|token| {
            let from = byte_to_utf16(token.span.start);
            let to = byte_to_utf16(token.span.end);
            json!({
                "kind": token_kind_name(token.kind),
                "from": from,
                "to": to,
            })
        })
        .collect::<Vec<_>>();

    // Diagnostic spans are converted independently because they overlap tokens
    // and therefore are not necessarily monotonic relative to the cursor above.
    let diagnostics = tokenization
        .diagnostics
        .iter()
        .map(|diagnostic| {
            json!({
                "from": source[..diagnostic.span.start].encode_utf16().count(),
                "to": source[..diagnostic.span.end].encode_utf16().count(),
                "code": diagnostic.code,
                "message": diagnostic.message,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "ok": true,
        "tokens": tokens,
        "diagnostics": diagnostics,
    })
}

const fn token_kind_name(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Property => "key",
        TokenKind::String => "string",
        TokenKind::Number => "number",
        TokenKind::Boolean => "boolean",
        TokenKind::Null => "null",
        TokenKind::Punctuation => "punctuation",
        TokenKind::Whitespace => "whitespace",
        TokenKind::Invalid => "invalid",
    }
}

fn filter_preview(
    input: &str,
    path: &str,
    filters: &[FilterCondition],
    filter_mode: FilterMode,
    source_format: SourceFormat,
    csv_delimiter: &str,
) -> Value {
    let value = match parse_input(input, source_format, csv_delimiter) {
        Ok(value) => value,
        Err(problem) => {
            return error_response(
                &format!("Некорректные исходные данные: {}", problem.message),
                problem.line,
                problem.column,
            )
        }
    };

    let selected = if path.is_empty() {
        &value
    } else {
        match value.pointer(path) {
            Some(selected) => selected,
            None => return error_response("Выбранный путь больше не существует", 0, 0),
        }
    };

    let Value::Array(items) = selected else {
        return error_response("По выбранному пути находится не массив", 0, 0);
    };

    let mut matched = Vec::new();
    let mut object_items = 0usize;
    let mut filtered_out = 0usize;
    let mut skipped_items = 0usize;

    for item in items {
        let Value::Object(object) = item else {
            skipped_items += 1;
            continue;
        };
        object_items += 1;

        if matches_filters(object, filters, filter_mode) {
            matched.push(item);
        } else {
            filtered_out += 1;
        }
    }

    json!({
        "ok": true,
        "input_json": serde_json::to_string_pretty(items).unwrap_or_else(|_| "[]".to_owned()),
        "output_json": serde_json::to_string_pretty(&matched).unwrap_or_else(|_| "[]".to_owned()),
        "source_items": items.len(),
        "object_items": object_items,
        "matched_items": matched.len(),
        "filtered_out": filtered_out,
        "skipped_items": skipped_items,
    })
}

fn parse_json(input: &str) -> Result<Value, ParseProblem> {
    serde_json::from_str(input).map_err(|error| ParseProblem {
        message: error.to_string(),
        line: error.line(),
        column: error.column(),
    })
}

fn parse_input(
    input: &str,
    source_format: SourceFormat,
    csv_delimiter: &str,
) -> Result<Value, ParseProblem> {
    match source_format {
        SourceFormat::Json => parse_json(input),
        SourceFormat::Csv => parse_csv(input, csv_delimiter),
        SourceFormat::List => parse_list(input),
    }
}

fn parse_list(input: &str) -> Result<Value, ParseProblem> {
    let mut values = Vec::new();
    let mut field = String::new();
    let mut quote: Option<char> = None;
    let mut line = 1usize;
    let mut column = 0usize;

    let push_field = |field: &mut String, values: &mut Vec<Value>| {
        let value = field.trim();
        if !value.is_empty() {
            values.push(json!({ "value": value }));
        }
        field.clear();
    };

    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        column += 1;
        if let Some(active_quote) = quote {
            if character == '\\' && characters.peek() == Some(&active_quote) {
                characters.next();
                column += 1;
                field.push(active_quote);
            } else if character == active_quote {
                quote = None;
            } else {
                field.push(character);
            }
            if character == '\n' {
                line += 1;
                column = 0;
            }
            continue;
        }

        match character {
            '\"' | '\'' if field.trim().is_empty() => quote = Some(character),
            ',' | '\n' => {
                push_field(&mut field, &mut values);
                if character == '\n' {
                    line += 1;
                    column = 0;
                }
            }
            '\r' => {}
            value => field.push(value),
        }
    }

    if quote.is_some() {
        return Err(ParseProblem {
            message: "Незакрытая кавычка в списке".to_owned(),
            line,
            column,
        });
    }
    push_field(&mut field, &mut values);
    Ok(Value::Array(values))
}

fn parse_csv(input: &str, delimiter: &str) -> Result<Value, ParseProblem> {
    let Some(separator) = delimiter.chars().next() else {
        return Err(ParseProblem {
            message: "Укажите разделитель CSV".to_owned(),
            line: 1,
            column: 1,
        });
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    let mut line = 1usize;
    let mut column = 0usize;

    while let Some(character) = chars.next() {
        column += 1;
        if quoted {
            if character == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    column += 1;
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                if character == '\n' {
                    line += 1;
                    column = 0;
                }
                field.push(character);
            }
            continue;
        }

        match character {
            '"' if field.is_empty() => quoted = true,
            value if value == separator => {
                row.push(std::mem::take(&mut field));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                line += 1;
                column = 0;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            value => field.push(value),
        }
    }

    if quoted {
        return Err(ParseProblem {
            message: "Незакрытая кавычка в CSV".to_owned(),
            line,
            column,
        });
    }
    row.push(field);
    if row.iter().any(|value| !value.is_empty()) || rows.is_empty() {
        rows.push(row);
    }

    let headers = rows.first().cloned().unwrap_or_default();
    if headers.is_empty() || headers.iter().all(|header| header.trim().is_empty()) {
        return Err(ParseProblem {
            message: "В CSV отсутствует строка заголовков".to_owned(),
            line: 1,
            column: 1,
        });
    }
    let headers: Vec<String> = headers
        .into_iter()
        .map(|header| header.trim().to_owned())
        .collect();
    let mut items = Vec::new();
    for values in rows.into_iter().skip(1) {
        if values.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let mut object = Map::new();
        for (index, header) in headers.iter().enumerate() {
            if header.is_empty() {
                continue;
            }
            object.insert(
                header.clone(),
                csv_scalar(values.get(index).map(String::as_str).unwrap_or("")),
            );
        }
        items.push(Value::Object(object));
    }
    Ok(Value::Array(items))
}

fn csv_scalar(raw: &str) -> Value {
    let value = raw.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if value.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(Value::Number(number)) = serde_json::from_str::<Value>(value) {
        return Value::Number(number);
    }
    Value::String(raw.to_owned())
}

fn analyze(input: &str, source_format: SourceFormat, csv_delimiter: &str) -> Value {
    let value = match parse_input(input, source_format, csv_delimiter) {
        Ok(value) => value,
        Err(problem) => {
            return error_response(
                &format!("Некорректные исходные данные: {}", problem.message),
                problem.line,
                problem.column,
            )
        }
    };

    let mut arrays = Vec::new();
    collect_arrays(&value, "", "$", &mut arrays);

    json!({
        "ok": true,
        "formatted_json": serde_json::to_string_pretty(&value).unwrap_or_else(|_| input.to_owned()),
        "root_type": value_kind(&value),
        "array_paths": arrays,
    })
}

fn collect_arrays(value: &Value, pointer: &str, label: &str, output: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            output.push(array_info(items, pointer, label));
        }
        Value::Object(object) => {
            for (key, child) in object {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                let next_pointer = format!("{pointer}/{escaped}");
                let next_label = if label == "$" {
                    key.to_owned()
                } else {
                    format!("{label}.{key}")
                };
                collect_arrays(child, &next_pointer, &next_label, output);
            }
        }
        _ => {}
    }
}

fn array_info(items: &[Value], pointer: &str, label: &str) -> Value {
    let mut fields: Vec<(String, String, usize)> = Vec::new();
    let mut object_items = 0usize;

    for item in items {
        let Value::Object(object) = item else {
            continue;
        };
        object_items += 1;

        for (name, value) in object {
            if let Some(existing) = fields.iter_mut().find(|field| field.0 == *name) {
                existing.2 += 1;
                let next_kind = value_kind(value).to_owned();
                if existing.1 == "null" && next_kind != "null" {
                    existing.1 = next_kind;
                } else if next_kind != "null" && existing.1 != next_kind {
                    existing.1 = "mixed".to_owned();
                }
            } else {
                fields.push((name.to_owned(), value_kind(value).to_owned(), 1));
            }
        }
    }

    let field_values: Vec<Value> = fields
        .into_iter()
        .map(|(name, kind, present)| {
            json!({
                "name": name,
                "kind": kind,
                "present": present,
            })
        })
        .collect();

    json!({
        "path": pointer,
        "label": label,
        "items": items.len(),
        "object_items": object_items,
        "skipped_items": items.len().saturating_sub(object_items),
        "fields": field_values,
    })
}

fn transform(
    input: &str,
    path: &str,
    fields: &[String],
    delimiter: &str,
    skip_empty: bool,
    unique: bool,
    filters: &[FilterCondition],
    filter_mode: FilterMode,
    source_format: SourceFormat,
    csv_delimiter: &str,
    output_format: OutputFormat,
    output_csv_delimiter: &str,
    csv_include_header: bool,
    csv_quote_all: bool,
    xml_root: &str,
    xml_row: &str,
    table_name: &str,
    value_template: &str,
    strip_outer_quotes: bool,
) -> Value {
    let value = match parse_input(input, source_format, csv_delimiter) {
        Ok(value) => value,
        Err(problem) => {
            return error_response(
                &format!("Некорректные исходные данные: {}", problem.message),
                problem.line,
                problem.column,
            )
        }
    };

    let selected = if path.is_empty() {
        &value
    } else {
        match value.pointer(path) {
            Some(selected) => selected,
            None => {
                return error_response("Выбранный путь больше не существует", 0, 0);
            }
        }
    };

    let Value::Array(items) = selected else {
        return error_response("По выбранному пути находится не массив", 0, 0);
    };

    let mut matched_objects: Vec<&Map<String, Value>> = Vec::new();
    let mut object_items = 0usize;
    let mut filtered_out = 0usize;
    let mut skipped_items = 0usize;
    let mut empty_values = 0usize;

    for item in items {
        let Value::Object(object) = item else {
            skipped_items += 1;
            continue;
        };
        object_items += 1;

        if !matches_filters(object, filters, filter_mode) {
            filtered_out += 1;
            continue;
        }
        matched_objects.push(object);
    }

    let (output, values_count) = match output_format {
        OutputFormat::Flat => format_flat(
            &matched_objects,
            fields,
            delimiter,
            skip_empty,
            unique,
            &mut empty_values,
        ),
        OutputFormat::Template => format_template(
            &matched_objects,
            fields,
            delimiter,
            value_template,
            strip_outer_quotes,
            skip_empty,
            unique,
            &mut empty_values,
        ),
        OutputFormat::Json => format_json(&matched_objects, fields, &mut empty_values),
        OutputFormat::Csv => format_csv(
            &matched_objects,
            fields,
            output_csv_delimiter,
            csv_include_header,
            csv_quote_all,
            &mut empty_values,
        ),
        OutputFormat::Xml => format_xml(
            &matched_objects,
            fields,
            xml_root,
            xml_row,
            &mut empty_values,
        ),
        OutputFormat::Sql => format_sql(&matched_objects, fields, table_name, &mut empty_values),
    };

    json!({
        "ok": true,
        "output": output,
        "source_items": items.len(),
        "object_items": object_items,
        "matched_items": matched_objects.len(),
        "filtered_out": filtered_out,
        "skipped_items": skipped_items,
        "empty_values": empty_values,
        "values": values_count,
    })
}

fn format_flat(
    objects: &[&Map<String, Value>],
    fields: &[String],
    delimiter: &str,
    skip_empty: bool,
    unique: bool,
    empty_values: &mut usize,
) -> (String, usize) {
    let mut values = Vec::new();
    let mut seen = HashSet::new();
    for object in objects {
        for field in fields {
            match object.get(field) {
                Some(Value::Null) | None => {
                    *empty_values += 1;
                    if !skip_empty {
                        push_value(String::new(), delimiter, unique, &mut seen, &mut values);
                    }
                }
                Some(value) => {
                    let raw = value_to_text(value);
                    if raw.is_empty() && skip_empty {
                        *empty_values += 1;
                    } else {
                        push_value(raw, delimiter, unique, &mut seen, &mut values);
                    }
                }
            }
        }
    }
    let count = values.len();
    (values.join(delimiter), count)
}

fn format_template(
    objects: &[&Map<String, Value>],
    fields: &[String],
    delimiter: &str,
    template: &str,
    strip_outer_quotes: bool,
    skip_empty: bool,
    unique: bool,
    empty_values: &mut usize,
) -> (String, usize) {
    let mut values = Vec::new();
    let mut seen = HashSet::new();
    for object in objects {
        for field in fields {
            match object.get(field) {
                Some(Value::Null) | None => {
                    *empty_values += 1;
                    if !skip_empty {
                        push_value(
                            template.replace("{value}", ""),
                            delimiter,
                            unique,
                            &mut seen,
                            &mut values,
                        );
                    }
                }
                Some(value) => {
                    let raw = value_to_text(value);
                    let raw = if strip_outer_quotes {
                        strip_matching_outer_quotes(&raw)
                    } else {
                        raw
                    };
                    if raw.is_empty() && skip_empty {
                        *empty_values += 1;
                    } else {
                        push_value(
                            template.replace("{value}", &raw),
                            delimiter,
                            unique,
                            &mut seen,
                            &mut values,
                        );
                    }
                }
            }
        }
    }
    let count = values.len();
    (values.join(delimiter), count)
}

fn strip_matching_outer_quotes(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    if first != '\"' && first != '\'' {
        return value.to_owned();
    }
    let Some(last) = value.chars().last() else {
        return value.to_owned();
    };
    if last != first || value.chars().count() < 2 {
        return value.to_owned();
    }
    value[first.len_utf8()..value.len() - last.len_utf8()].to_owned()
}

fn projected_object(
    object: &Map<String, Value>,
    fields: &[String],
    empty_values: &mut usize,
) -> Value {
    let mut projected = Map::new();
    for field in fields {
        let value = object.get(field).cloned().unwrap_or(Value::Null);
        if value.is_null() {
            *empty_values += 1;
        }
        projected.insert(field.clone(), value);
    }
    Value::Object(projected)
}

fn format_json(
    objects: &[&Map<String, Value>],
    fields: &[String],
    empty_values: &mut usize,
) -> (String, usize) {
    let rows: Vec<Value> = objects
        .iter()
        .map(|object| projected_object(object, fields, empty_values))
        .collect();
    let count = rows.len() * fields.len();
    (
        serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_owned()),
        count,
    )
}

fn format_csv(
    objects: &[&Map<String, Value>],
    fields: &[String],
    delimiter: &str,
    include_header: bool,
    quote_all: bool,
    empty_values: &mut usize,
) -> (String, usize) {
    let separator = delimiter.chars().next().unwrap_or(',').to_string();
    let mut rows = Vec::new();
    if include_header {
        rows.push(
            fields
                .iter()
                .map(|field| escape_csv_value(field, &separator, quote_all))
                .collect::<Vec<_>>()
                .join(&separator),
        );
    }
    for object in objects {
        let row = fields
            .iter()
            .map(|field| {
                let value = object.get(field).unwrap_or(&Value::Null);
                if value.is_null() {
                    *empty_values += 1;
                }
                escape_csv_value(&value_to_text(value), &separator, quote_all)
            })
            .collect::<Vec<_>>()
            .join(&separator);
        rows.push(row);
    }
    (rows.join("\n"), objects.len() * fields.len())
}

fn format_xml(
    objects: &[&Map<String, Value>],
    fields: &[String],
    root_name: &str,
    row_name: &str,
    empty_values: &mut usize,
) -> (String, usize) {
    let root = xml_name(root_name, "rows");
    let row = xml_name(row_name, "row");
    let mut lines = vec![
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_owned(),
        format!("<{root}>"),
    ];
    for object in objects {
        lines.push(format!("  <{row}>"));
        for field in fields {
            let tag = xml_name(field, "field");
            let value = object.get(field).unwrap_or(&Value::Null);
            if value.is_null() {
                *empty_values += 1;
            }
            lines.push(format!(
                "    <{tag}>{}</{tag}>",
                xml_escape(&value_to_text(value))
            ));
        }
        lines.push(format!("  </{row}>"));
    }
    lines.push(format!("</{root}>"));
    (lines.join("\n"), objects.len() * fields.len())
}

fn xml_name(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    let mut output = String::new();
    for (index, character) in trimmed.chars().enumerate() {
        let valid = character.is_alphanumeric() || matches!(character, '_' | '-' | '.');
        if index == 0 && !(character.is_alphabetic() || character == '_') {
            output.push('_');
        }
        output.push(if valid { character } else { '_' });
    }
    output
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn format_sql(
    objects: &[&Map<String, Value>],
    fields: &[String],
    table_name: &str,
    empty_values: &mut usize,
) -> (String, usize) {
    let table = quote_identifier(if table_name.trim().is_empty() {
        "result"
    } else {
        table_name.trim()
    });
    let columns = fields
        .iter()
        .map(|field| quote_identifier(field))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = objects
        .iter()
        .map(|object| {
            let values = fields
                .iter()
                .map(|field| {
                    let value = object.get(field).unwrap_or(&Value::Null);
                    if value.is_null() {
                        *empty_values += 1;
                    }
                    sql_value(value)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {table} ({columns}) VALUES ({values});")
        })
        .collect::<Vec<_>>();
    (rows.join("\n"), objects.len() * fields.len())
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn sql_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(boolean) => {
            if *boolean {
                "TRUE".to_owned()
            } else {
                "FALSE".to_owned()
            }
        }
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("'{}'", text.replace('\'', "''")),
        Value::Array(_) | Value::Object(_) => {
            let text = serde_json::to_string(value).unwrap_or_default();
            format!("'{}'", text.replace('\'', "''"))
        }
    }
}

fn matches_filters(
    object: &Map<String, Value>,
    filters: &[FilterCondition],
    mode: FilterMode,
) -> bool {
    if filters.is_empty() {
        return true;
    }

    match mode {
        FilterMode::All => filters
            .iter()
            .all(|condition| matches_condition(object, condition)),
        FilterMode::Any => filters
            .iter()
            .any(|condition| matches_condition(object, condition)),
    }
}

fn matches_condition(object: &Map<String, Value>, condition: &FilterCondition) -> bool {
    matches_value(
        object.get(&condition.field),
        condition.operator,
        &condition.value,
    )
}

fn matches_value(actual: Option<&Value>, operator: FilterOperator, expected: &str) -> bool {
    match operator {
        FilterOperator::Exists => actual.is_some(),
        FilterOperator::NotExists => actual.is_none(),
        FilterOperator::Equal => actual
            .and_then(|value| values_equal(value, expected))
            .unwrap_or(false),
        FilterOperator::NotEqual => actual
            .and_then(|value| values_equal(value, expected))
            .map(|is_equal| !is_equal)
            .unwrap_or(false),
        FilterOperator::GreaterThan => compare_value(actual, expected)
            .map(|ordering| ordering == Ordering::Greater)
            .unwrap_or(false),
        FilterOperator::GreaterOrEqual => compare_value(actual, expected)
            .map(|ordering| matches!(ordering, Ordering::Greater | Ordering::Equal))
            .unwrap_or(false),
        FilterOperator::LessThan => compare_value(actual, expected)
            .map(|ordering| ordering == Ordering::Less)
            .unwrap_or(false),
        FilterOperator::LessOrEqual => compare_value(actual, expected)
            .map(|ordering| matches!(ordering, Ordering::Less | Ordering::Equal))
            .unwrap_or(false),
        FilterOperator::Contains => actual
            .map(|value| filter_text(value).contains(&expected_string(expected)))
            .unwrap_or(false),
        FilterOperator::StartsWith => actual
            .map(|value| filter_text(value).starts_with(&expected_string(expected)))
            .unwrap_or(false),
        FilterOperator::EndsWith => actual
            .map(|value| filter_text(value).ends_with(&expected_string(expected)))
            .unwrap_or(false),
    }
}

fn values_equal(actual: &Value, expected: &str) -> Option<bool> {
    match actual {
        Value::String(value) => Some(value == &expected_string(expected)),
        Value::Number(value) => {
            compare_numbers(value, expected).map(|ordering| ordering == Ordering::Equal)
        }
        Value::Bool(value) => expected
            .trim()
            .parse::<bool>()
            .ok()
            .map(|expected_bool| *value == expected_bool),
        Value::Null => Some(expected.trim().eq_ignore_ascii_case("null")),
        Value::Array(_) | Value::Object(_) => serde_json::from_str::<Value>(expected.trim())
            .ok()
            .map(|expected_value| actual == &expected_value),
    }
}

fn compare_value(actual: Option<&Value>, expected: &str) -> Option<Ordering> {
    match actual? {
        Value::Number(value) => compare_numbers(value, expected),
        Value::String(value) => Some(value.as_str().cmp(expected_string(expected).as_str())),
        Value::Bool(value) => Some(value.cmp(&expected.trim().parse::<bool>().ok()?)),
        _ => None,
    }
}

fn compare_numbers(actual: &serde_json::Number, expected: &str) -> Option<Ordering> {
    let Value::Number(expected) = serde_json::from_str::<Value>(expected.trim()).ok()? else {
        return None;
    };

    if let (Some(actual), Some(expected)) = (actual.as_i64(), expected.as_i64()) {
        return Some(actual.cmp(&expected));
    }
    if let (Some(actual), Some(expected)) = (actual.as_u64(), expected.as_u64()) {
        return Some(actual.cmp(&expected));
    }
    if let (Some(actual), Some(expected)) = (actual.as_i64(), expected.as_u64()) {
        return Some(if actual < 0 {
            Ordering::Less
        } else {
            (actual as u64).cmp(&expected)
        });
    }
    if let (Some(actual), Some(expected)) = (actual.as_u64(), expected.as_i64()) {
        return Some(if expected < 0 {
            Ordering::Greater
        } else {
            actual.cmp(&(expected as u64))
        });
    }

    actual.as_f64()?.partial_cmp(&expected.as_f64()?)
}

fn expected_string(value: &str) -> String {
    value.to_owned()
}

fn filter_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_owned(),
        _ => value_to_text(value),
    }
}

fn push_value(
    raw: String,
    delimiter: &str,
    unique: bool,
    seen: &mut HashSet<String>,
    values: &mut Vec<String>,
) {
    if unique && !seen.insert(raw.clone()) {
        return;
    }
    values.push(escape_value(&raw, delimiter));
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_owned(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| String::new())
        }
    }
}

fn escape_value(value: &str, delimiter: &str) -> String {
    escape_csv_value(value, delimiter, false)
}

fn escape_csv_value(value: &str, delimiter: &str, quote_all: bool) -> String {
    let needs_quotes = quote_all
        || value.contains('"')
        || value.contains('\n')
        || value.contains('\r')
        || (!delimiter.is_empty() && value.contains(delimiter));

    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn error_response(message: &str, line: usize, column: usize) -> Value {
    json!({
        "ok": false,
        "error": {
            "message": message,
            "line": line,
            "column": column,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "stores": [
            {"id":"A1","name":"Москва 4-10","state":1},
            {"id":"B2","name":"Белгород-2","state":1}
        ]
    }"#;

    fn request(value: Value) -> Value {
        serde_json::from_str(&process_request(&value.to_string())).unwrap()
    }

    #[test]
    fn exposes_rust_tokenizer_with_browser_utf16_offsets() {
        let result = request(json!({
            "action": "tokenize_json",
            "source": "{\"emoji\":\"😀\"}"
        }));

        assert_eq!(result["ok"], true);
        assert_eq!(result["tokens"][1]["kind"], "key");
        assert_eq!(result["tokens"][3]["kind"], "string");
        assert_eq!(result["tokens"][3]["from"], 9);
        assert_eq!(result["tokens"][3]["to"], 13);
        assert_eq!(result["diagnostics"], json!([]));
    }

    #[test]
    fn discovers_array_and_union_of_fields() {
        let result = request(json!({"action": "analyze", "json": SAMPLE}));
        assert_eq!(result["ok"], true);
        assert_eq!(result["array_paths"][0]["path"], "/stores");
        assert_eq!(result["array_paths"][0]["items"], 2);
        assert_eq!(result["array_paths"][0]["fields"][1]["name"], "name");
    }

    #[test]
    fn flattens_a_selected_field() {
        let result = request(json!({
            "action": "transform",
            "json": SAMPLE,
            "path": "/stores",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false
        }));
        assert_eq!(result["output"], "Москва 4-10, Белгород-2");
        assert_eq!(result["values"], 2);
        assert_eq!(result["source_items"], 2);
        assert_eq!(result["object_items"], 2);
        assert_eq!(result["matched_items"], 2);
        assert_eq!(result["filtered_out"], 0);
        assert_eq!(result["skipped_items"], 0);
    }

    #[test]
    fn preserves_item_then_field_order() {
        let result = request(json!({
            "action": "transform",
            "json": SAMPLE,
            "path": "/stores",
            "fields": ["id", "name"],
            "delimiter": " | ",
            "skip_empty": true,
            "unique": false
        }));
        assert_eq!(result["output"], "A1 | Москва 4-10 | B2 | Белгород-2");
    }

    #[test]
    fn skips_nulls_and_deduplicates() {
        let input = r#"[{"x":"one"},{"x":null},{"x":"one"},{"x":"two"}]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["x"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": true
        }));
        assert_eq!(result["output"], "one, two");
        assert_eq!(result["empty_values"], 1);
    }

    #[test]
    fn reports_invalid_json_location() {
        let result = request(json!({"action": "analyze", "json": "{\"x\": ]"}));
        assert_eq!(result["ok"], false);
        assert!(result["error"]["line"].as_u64().unwrap() > 0);
        assert!(result["error"]["column"].as_u64().unwrap() > 0);
    }

    #[test]
    fn quotes_values_that_collide_with_separator() {
        let input = r#"[{"x":"a, b"},{"x":"say \"hi\""}]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["x"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false
        }));
        assert_eq!(result["output"], "\"a, b\", \"say \"\"hi\"\"\"");
    }

    #[test]
    fn filters_numeric_fields_before_flattening() {
        let input = r#"[
            {"name":"Активный","state":1},
            {"name":"Скрытый","state":0},
            {"name":"Без статуса"}
        ]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"state","operator":"equal","value":"1"}],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "Активный");
        assert_eq!(result["matched_items"], 1);
        assert_eq!(result["filtered_out"], 2);
    }

    #[test]
    fn previews_items_before_and_after_filtering() {
        let input = r#"{
            "meta":{"page":1},
            "stores":[
                {"id":9007199254740993,"name":"Активный","state":1,"details":{"city":"Москва"}},
                {"id":2,"name":"Скрытый","state":0},
                {"id":3,"name":"Без статуса"},
                "не объект"
            ]
        }"#;
        let result = request(json!({
            "action": "filter_preview",
            "json": input,
            "path": "/stores",
            "filters": [{"field":"state","operator":"equal","value":"1"}],
            "filter_mode": "all"
        }));

        let before: Value = serde_json::from_str(result["input_json"].as_str().unwrap()).unwrap();
        let after: Value = serde_json::from_str(result["output_json"].as_str().unwrap()).unwrap();
        assert_eq!(before.as_array().unwrap().len(), 4);
        assert_eq!(
            after,
            json!([{
                "id": 9007199254740993u64,
                "name": "Активный",
                "state": 1,
                "details": {"city": "Москва"}
            }])
        );
        assert_eq!(after[0]["name"], "Активный");
        assert!(result["input_json"]
            .as_str()
            .unwrap()
            .contains("9007199254740993"));
        assert!(result["output_json"]
            .as_str()
            .unwrap()
            .contains("9007199254740993"));
        assert_eq!(result["source_items"], 4);
        assert_eq!(result["object_items"], 3);
        assert_eq!(result["matched_items"], 1);
        assert_eq!(result["filtered_out"], 2);
        assert_eq!(result["skipped_items"], 1);
    }

    #[test]
    fn preview_returns_an_empty_array_when_nothing_matches() {
        let result = request(json!({
            "action": "filter_preview",
            "json": r#"[{"state":1},{"state":0}]"#,
            "path": "",
            "filters": [{"field":"state","operator":"equal","value":"2"}],
            "filter_mode": "all"
        }));

        assert_eq!(result["output_json"], "[]");
        assert_eq!(result["matched_items"], 0);
        assert_eq!(result["filtered_out"], 2);
    }

    #[test]
    fn combines_conditions_with_any_mode() {
        let input = r#"[
            {"name":"Москва","state":1,"locality":"Москва"},
            {"name":"Белгород","state":0,"locality":"Белгород"},
            {"name":"Курск","state":0,"locality":"Курск"}
        ]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [
                {"field":"state","operator":"equal","value":"1"},
                {"field":"locality","operator":"contains","value":"город"}
            ],
            "filter_mode": "any"
        }));
        assert_eq!(result["output"], "Москва, Белгород");
        assert_eq!(result["matched_items"], 2);
    }

    #[test]
    fn distinguishes_present_null_from_missing() {
        let input = r#"[{"name":"null","x":null},{"name":"missing"}]"#;
        let exists = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"x","operator":"exists"}],
            "filter_mode": "all"
        }));
        let missing = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"x","operator":"not_exists"}],
            "filter_mode": "all"
        }));
        assert_eq!(exists["output"], "null");
        assert_eq!(missing["output"], "missing");
    }

    #[test]
    fn missing_field_does_not_match_not_equal() {
        let input = r#"[{"name":"zero","state":0},{"name":"missing"}]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"state","operator":"not_equal","value":"1"}],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "zero");
        assert_eq!(result["matched_items"], 1);
    }

    #[test]
    fn filtering_happens_before_empty_and_value_accounting() {
        let input = r#"[
            {"id":"keep-1","status":"open","payload":null},
            17,
            {"id":"drop","status":"closed"},
            {"id":"keep-2","status":"open"}
        ]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["id", "payload"],
            "delimiter": " | ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"status","operator":"equal","value":"open"}],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "keep-1 | keep-2");
        assert_eq!(result["source_items"], 4);
        assert_eq!(result["object_items"], 3);
        assert_eq!(result["matched_items"], 2);
        assert_eq!(result["filtered_out"], 1);
        assert_eq!(result["skipped_items"], 1);
        assert_eq!(result["empty_values"], 2);
        assert_eq!(result["values"], 2);
    }

    #[test]
    fn preserves_large_integer_precision() {
        let input = r#"[
            {"id":"exact","n":9007199254740993},
            {"id":"adjacent","n":9007199254740992}
        ]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["id"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{
                "field":"n",
                "operator":"equal",
                "value":"9007199254740993"
            }],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "exact");
        assert_eq!(result["matched_items"], 1);
    }

    #[test]
    fn invalid_numeric_operand_never_broad_matches() {
        let input = r#"[{"id":"one","state":1},{"id":"zero","state":0}]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["id"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"state","operator":"not_equal","value":"abc"}],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "");
        assert_eq!(result["matched_items"], 0);
        assert_eq!(result["filtered_out"], 2);
    }

    #[test]
    fn analyzes_and_filters_csv_input() {
        let csv = "id,name,state\nA1,Москва,1\nB2,Белгород,0";
        let analysis = request(json!({
            "action": "analyze",
            "json": csv,
            "source_format": "csv",
            "csv_delimiter": ","
        }));
        assert_eq!(analysis["ok"], true);
        assert_eq!(analysis["array_paths"][0]["path"], "");
        assert_eq!(analysis["array_paths"][0]["fields"][2]["kind"], "number");

        let result = request(json!({
            "action": "transform",
            "json": csv,
            "source_format": "csv",
            "csv_delimiter": ",",
            "path": "",
            "fields": ["name"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "filters": [{"field":"state","operator":"equal","value":"1"}],
            "filter_mode": "all"
        }));
        assert_eq!(result["output"], "Москва");
        assert_eq!(result["matched_items"], 1);
    }

    #[test]
    fn exports_projected_rows_as_json_and_csv() {
        let input = r#"[{"name":"Москва","state":1},{"name":"Белгород","state":0}]"#;
        let json_result = request(json!({
            "action": "transform", "json": input, "path": "", "fields": ["name", "state"],
            "delimiter": ", ", "skip_empty": true, "unique": false,
            "output_format": "json", "filters": [], "filter_mode": "all"
        }));
        let output: Value = serde_json::from_str(json_result["output"].as_str().unwrap()).unwrap();
        assert_eq!(output[0]["name"], "Москва");

        let csv_result = request(json!({
            "action": "transform", "json": input, "path": "", "fields": ["name", "state"],
            "delimiter": ", ", "skip_empty": true, "unique": false,
            "output_format": "csv", "output_csv_delimiter": ";", "filters": [], "filter_mode": "all"
        }));
        assert_eq!(csv_result["output"], "name;state\nМосква;1\nБелгород;0");
    }

    #[test]
    fn configures_csv_header_and_quoting() {
        let input = r#"[{"name":"Москва","state":1},{"name":"Белгород","state":0}]"#;
        let result = request(json!({
            "action": "transform", "json": input, "path": "", "fields": ["name", "state"],
            "delimiter": ", ", "skip_empty": true, "unique": false,
            "output_format": "csv", "output_csv_delimiter": ";",
            "csv_include_header": false, "csv_quote_all": true,
            "filters": [], "filter_mode": "all"
        }));
        assert_eq!(result["output"], "\"Москва\";\"1\"\n\"Белгород\";\"0\"");
    }

    #[test]
    fn exports_safe_sql_insert_statements() {
        let input = r#"[{"name":"O'Reilly","state":1},{"name":"Пусто","state":null}]"#;
        let result = request(json!({
            "action": "transform", "json": input, "path": "", "fields": ["name", "state"],
            "delimiter": ", ", "skip_empty": true, "unique": false,
            "output_format": "sql", "table_name": "stores", "filters": [], "filter_mode": "all"
        }));
        assert_eq!(
            result["output"],
            "INSERT INTO \"stores\" (\"name\", \"state\") VALUES ('O''Reilly', 1);\nINSERT INTO \"stores\" (\"name\", \"state\") VALUES ('Пусто', NULL);"
        );
    }

    #[test]
    fn exports_xml_with_safe_tags_and_escaped_values() {
        let input =
            r#"[{"store name":"A & B <Москва>","state":1},{"store name":"Белгород","state":null}]"#;
        let result = request(json!({
            "action": "transform",
            "json": input,
            "path": "",
            "fields": ["store name", "state"],
            "delimiter": ", ",
            "skip_empty": true,
            "unique": false,
            "output_format": "xml",
            "xml_root": "stores",
            "xml_row": "store",
            "filters": [],
            "filter_mode": "all"
        }));
        let xml = result["output"].as_str().unwrap();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<stores>"));
        assert!(xml.contains("<store_name>A &amp; B &lt;Москва&gt;</store_name>"));
        assert!(xml.contains("<state></state>"));
        assert!(xml.ends_with("</stores>"));
        assert_eq!(result["empty_values"], 1);
    }
}
