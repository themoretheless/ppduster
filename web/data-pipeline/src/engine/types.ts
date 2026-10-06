export interface EngineError {
  message: string;
  line: number;
  column: number;
}

export interface FieldInfo {
  name: string;
  kind: "string" | "number" | "boolean" | "object" | "array" | "null" | "mixed";
  present: number;
}

export interface ArrayInfo {
  path: string;
  label: string;
  items: number;
  object_items: number;
  skipped_items: number;
  fields: FieldInfo[];
}

export type FilterMode = "all" | "any";
export type SourceFormat = "json" | "csv" | "list";
export type OutputFormat = "flat" | "template" | "json" | "csv" | "xml" | "sql";

export type FilterOperator =
  | "equal"
  | "not_equal"
  | "greater_than"
  | "greater_or_equal"
  | "less_than"
  | "less_or_equal"
  | "contains"
  | "starts_with"
  | "ends_with"
  | "exists"
  | "not_exists";

export interface FilterCondition {
  field: string;
  operator: FilterOperator;
  value: string;
}

import type { ExecutePlanRequest } from "../runtime/execute-plan.ts";

export type EngineRequest =
  | ExecutePlanRequest
  | {
      action: "tokenize_json";
      source: string;
    }
  | {
      action: "analyze";
      json: string;
      source_format: SourceFormat;
      csv_delimiter: string;
    }
  | {
      action: "filter_preview";
      json: string;
      path: string;
      filters: FilterCondition[];
      filter_mode: FilterMode;
      source_format?: SourceFormat;
      csv_delimiter?: string;
    }
  | {
      action: "transform";
      json: string;
      path: string;
      fields: string[];
      delimiter: string;
      skip_empty: boolean;
      unique: boolean;
      filters: FilterCondition[];
      filter_mode: FilterMode;
      source_format: SourceFormat;
      csv_delimiter: string;
      output_format: OutputFormat;
      output_csv_delimiter: string;
      csv_include_header: boolean;
      csv_quote_all: boolean;
      xml_root: string;
      xml_row: string;
      table_name: string;
      value_template: string;
      strip_outer_quotes: boolean;
    };

export interface AnalyzeSuccess {
  ok: true;
  formatted_json: string;
  root_type: string;
  array_paths: ArrayInfo[];
}

export type JsonSyntaxTokenKind =
  | "key"
  | "string"
  | "number"
  | "boolean"
  | "null"
  | "punctuation"
  | "whitespace"
  | "invalid";

export interface JsonSyntaxToken {
  kind: JsonSyntaxTokenKind;
  /** UTF-16 offsets, matching textarea selection offsets. */
  from: number;
  to: number;
}

export interface JsonSyntaxDiagnostic {
  from: number;
  to: number;
  code: string;
  message: string;
}

export interface TokenizeJsonSuccess {
  ok: true;
  tokens: JsonSyntaxToken[];
  diagnostics: JsonSyntaxDiagnostic[];
}

export type TokenizeJsonResponse = TokenizeJsonSuccess | EngineFailure;

export interface TransformSuccess {
  ok: true;
  output: string;
  source_items: number;
  object_items: number;
  matched_items: number;
  filtered_out: number;
  skipped_items: number;
  empty_values: number;
  values: number;
}

export interface FilterPreviewSuccess {
  ok: true;
  input_json: string;
  output_json: string;
  source_items: number;
  object_items: number;
  matched_items: number;
  filtered_out: number;
  skipped_items: number;
}

export interface EngineFailure {
  ok: false;
  error: EngineError;
}

export type AnalyzeResponse = AnalyzeSuccess | EngineFailure;
export type FilterPreviewResponse = FilterPreviewSuccess | EngineFailure;
export type TransformResponse = TransformSuccess | EngineFailure;

export interface TimedResponse<T> {
  response: T;
  durationMs: number;
  engineVersion: string;
}
