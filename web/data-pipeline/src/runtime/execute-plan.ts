import {
  BUILTIN_NODE_REGISTRY,
  createGraphIndex,
  endpointKey,
  stableTopologicalSort,
  validateGraph,
  type GraphDocument,
  type GraphIssue,
  type GraphNode,
  type JsonValue,
} from "../graph-v2/index.ts";
import {
  legacyConditionsToExpression,
  normalizeFilterExpression,
  type FilterExpression,
} from "./filter-expression.ts";
import { computeExecutePlanStepCacheKey } from "./cache-key.ts";

export type PlanSourceFormat = "json" | "csv" | "list";
export type PlanFilterMode = "all" | "any";
export type PlanSinkFormat = "flat" | "template" | "json" | "csv" | "xml" | "sql";
export type PlanFilterOperator =
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

export interface PlanFilterCondition {
  readonly field: string;
  readonly quantifier?: "one" | "any" | "all" | "none";
  readonly operator: PlanFilterOperator;
  readonly value?: string;
}

export interface SourceStepConfig {
  readonly data: string;
  readonly format: PlanSourceFormat;
  readonly path: string;
  readonly csv_delimiter: string;
}

export interface FilterStepConfig {
  readonly filters: readonly PlanFilterCondition[];
  readonly filter_mode: PlanFilterMode;
  /** Preferred nested predicate; filters/filter_mode remain for v1 compatibility. */
  readonly expression?: FilterExpression;
}

export interface ProjectStepConfig {
  readonly fields: readonly string[];
}

export interface TemplateStepConfig {
  readonly value_template: string;
  readonly strip_outer_quotes: boolean;
  readonly skip_empty: boolean;
}

export interface SinkStepConfig {
  readonly format: PlanSinkFormat;
  readonly fields?: readonly string[];
  readonly delimiter: string;
  readonly skip_empty: boolean;
  readonly unique: boolean;
  readonly csv_delimiter: string;
  readonly csv_include_header: boolean;
  readonly csv_quote_all: boolean;
  readonly xml_root: string;
  readonly xml_row: string;
  readonly table_name: string;
  readonly value_template: string;
  readonly strip_outer_quotes: boolean;
}

export interface ExecutePlanInput {
  readonly node_id: string;
  /** The actual named output port in Graph v2 (`records`, `matched`, etc.). */
  readonly port: string;
}

interface ExecutePlanStepBase {
  readonly node_id: string;
  readonly cache_key: string;
  readonly input?: ExecutePlanInput;
}

export interface ExecuteSourceStep extends ExecutePlanStepBase {
  readonly node_type: "source";
  readonly input?: never;
  readonly config: SourceStepConfig;
}

export interface ExecuteFilterStep extends ExecutePlanStepBase {
  readonly node_type: "filter";
  readonly input: ExecutePlanInput;
  readonly config: FilterStepConfig;
}

export interface ExecuteProjectStep extends ExecutePlanStepBase {
  readonly node_type: "project";
  readonly input: ExecutePlanInput;
  readonly config: ProjectStepConfig;
}

export interface ExecuteTemplateStep extends ExecutePlanStepBase {
  readonly node_type: "template";
  readonly input: ExecutePlanInput;
  readonly config: TemplateStepConfig;
}

export interface ExecuteSinkStep extends ExecutePlanStepBase {
  readonly node_type: "sink";
  readonly input: ExecutePlanInput;
  readonly config: SinkStepConfig;
}

export type ExecutePlanStep =
  | ExecuteSourceStep
  | ExecuteFilterStep
  | ExecuteProjectStep
  | ExecuteTemplateStep
  | ExecuteSinkStep;

export interface ExecutePlanRequest {
  readonly action: "execute_plan";
  readonly plan: {
    readonly version: 1;
    readonly graph_id: string;
    readonly graph_revision: number;
    readonly preview_limit: number;
    readonly steps: readonly ExecutePlanStep[];
  };
}

export interface ExecutePlanDiagnostic {
  readonly severity: "error" | "warning";
  readonly code: string;
  readonly message: string;
  readonly node_id?: string | null;
  readonly expression_path?: string;
}

export interface ExecutePlanNodeStats {
  readonly input_items: number;
  readonly output_items: number;
  readonly filtered_out: number;
  readonly skipped_items: number;
  readonly empty_values?: number;
  readonly values?: number;
}

export interface ExecutePlanFieldSchema {
  readonly name: string;
  readonly kind: string;
  readonly present: number;
}

export interface ExecutePlanRecordSchema {
  readonly kind: "records";
  readonly fields: readonly ExecutePlanFieldSchema[];
}

export interface ExecutePlanValueVectorSchema {
  readonly kind: "value-vector";
  readonly value_type: "string";
}

export type ExecutePlanDataSchema = ExecutePlanRecordSchema | ExecutePlanValueVectorSchema;

export interface ExecutePlanNodeResult {
  readonly ok: boolean;
  readonly cached: boolean;
  readonly preview: readonly JsonValue[];
  readonly preview_truncated: boolean;
  /** @deprecated Compatibility alias for output_schema during the transition. */
  readonly schema: ExecutePlanDataSchema | null;
  /** Schema read from this node's direct input binding. */
  readonly input_schema: ExecutePlanDataSchema | null;
  /** Schema physically emitted by this node. */
  readonly output_schema: ExecutePlanDataSchema | null;
  readonly stats: ExecutePlanNodeStats;
  readonly diagnostics: readonly ExecutePlanDiagnostic[];
}

export interface ExecutePlanResponse {
  readonly ok: boolean;
  readonly nodes: Readonly<Record<string, ExecutePlanNodeResult>>;
  readonly sink_outputs: Readonly<Record<string, string>>;
  readonly diagnostics: readonly ExecutePlanDiagnostic[];
}

/**
 * Structural subset of the v1 canvas node. This intentionally does not import
 * App.vue's private FlowNode type, so persisted v1 documents can be adapted in
 * migrations, workers, and tests.
 */
export interface LegacyFlowLikeNode {
  readonly id: string;
  readonly json?: string;
  readonly sourceFormat?: PlanSourceFormat;
  readonly csvDelimiter?: string;
  readonly selectedPath?: string;
  readonly selectedFields?: readonly string[];
  readonly conditions?: readonly (PlanFilterCondition & { readonly id?: number })[];
  readonly filterMode?: PlanFilterMode;
  readonly filterExpression?: FilterExpression;
  readonly delimiter?: string;
  readonly outputFormat?: PlanSinkFormat;
  readonly csvIncludeHeader?: boolean;
  readonly csvQuoteAll?: boolean;
  readonly xmlRoot?: string;
  readonly xmlRow?: string;
  readonly tableName?: string;
  readonly valueTemplate?: string;
  readonly stripOuterQuotes?: boolean;
}

export interface BuildExecutionPlanOptions {
  /** Optional v1 configs, keyed by their node id, used while migrating old files. */
  readonly legacyNodes?: readonly LegacyFlowLikeNode[];
  readonly previewLimit?: number;
}

export class ExecutionPlanCompileError extends Error {
  readonly issues: readonly GraphIssue[];

  constructor(issues: readonly GraphIssue[]) {
    super(issues.map((item) => item.message).join("; ") || "Граф нельзя выполнить");
    this.name = "ExecutionPlanCompileError";
    this.issues = issues;
  }
}

const FILTER_OPERATORS = new Set<PlanFilterOperator>([
  "equal", "not_equal", "greater_than", "greater_or_equal", "less_than",
  "less_or_equal", "contains", "starts_with", "ends_with", "exists", "not_exists",
]);

const isRecord = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const stringValue = (value: unknown, fallback = ""): string =>
  typeof value === "string" ? value : fallback;

const booleanValue = (value: unknown, fallback: boolean): boolean =>
  typeof value === "boolean" ? value : fallback;

const oneCharacter = (value: unknown, fallback: string): string =>
  stringValue(value).charAt(0) || fallback;

const stringList = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];

const legacyRecord = (node: LegacyFlowLikeNode | undefined): Readonly<Record<string, unknown>> =>
  (node ?? {}) as Readonly<Record<string, unknown>>;

const mergedConfig = (
  node: GraphNode,
  legacy: LegacyFlowLikeNode | undefined,
): Readonly<Record<string, unknown>> => {
  const defaults = BUILTIN_NODE_REGISTRY.get(node.type)?.defaultConfig ?? {};
  return { ...defaults, ...node.config, ...legacyRecord(legacy) };
};

const parseConditions = (value: unknown): PlanFilterCondition[] => {
  if (!Array.isArray(value)) return [];
  return value.flatMap((candidate) => {
    if (!isRecord(candidate)) return [];
    const operator = candidate.operator;
    if (typeof operator !== "string" || !FILTER_OPERATORS.has(operator as PlanFilterOperator)) return [];
    const condition: PlanFilterCondition = {
      field: stringValue(candidate.field),
      operator: operator as PlanFilterOperator,
    };
    return typeof candidate.value === "string"
      ? [{ ...condition, value: candidate.value }]
      : [condition];
  });
};

const sourceConfig = (
  node: GraphNode,
  config: Readonly<Record<string, unknown>>,
): SourceStepConfig => ({
  // Legacy keys precede built-in defaults so an empty default `text` does not
  // mask the actual v1 `json` payload.
  data: stringValue(config.data, stringValue(config.json, stringValue(config.text))),
  format: node.type === "source.list" || config.format === "list" || config.sourceFormat === "list"
    ? "list"
    : node.type === "source.csv" || config.format === "csv" || config.sourceFormat === "csv"
      ? "csv"
      : "json",
  path: stringValue(config.path, stringValue(config.selectedPath, stringValue(config.arrayPath))),
  csv_delimiter: oneCharacter(config.csv_delimiter ?? config.csvDelimiter ?? config.delimiter, ","),
});

const filterConfig = (nodeId: string, config: Readonly<Record<string, unknown>>): FilterStepConfig => {
  const filters = parseConditions(config.filters ?? config.conditions);
  const filterMode: PlanFilterMode =
    config.filter_mode === "any" || config.mode === "any" || config.filterMode === "any"
      ? "any"
      : "all";
  const rawExpression = config.expression ?? config.filterExpression ?? config.filter_ast;
  const configuredExpression = normalizeFilterExpression(rawExpression);
  if (rawExpression !== undefined && rawExpression !== null && !configuredExpression) {
    throw new ExecutionPlanCompileError([{
      code: "invalid_node_config",
      severity: "error",
      nodeId,
      message: `В блоке «${nodeId}» некорректное или слишком сложное выражение фильтра`,
    }]);
  }
  const expression = configuredExpression ?? legacyConditionsToExpression(filters, filterMode);
  return {
    filters,
    filter_mode: filterMode,
    ...(expression ? { expression } : {}),
  };
};

const projectConfig = (config: Readonly<Record<string, unknown>>): ProjectStepConfig => ({
  fields: stringList(config.selectedFields ?? config.fields),
});

const templateConfig = (config: Readonly<Record<string, unknown>>): TemplateStepConfig => ({
  value_template: stringValue(
    config.value_template,
    stringValue(config.valueTemplate, stringValue(config.template, "0x{value}")),
  ),
  strip_outer_quotes: booleanValue(config.strip_outer_quotes ?? config.stripOuterQuotes, true),
  skip_empty: booleanValue(config.skip_empty ?? config.skipEmpty, true),
});

const sinkFormat = (node: GraphNode, config: Readonly<Record<string, unknown>>): PlanSinkFormat => {
  const fromType = node.type.startsWith("sink.") ? node.type.slice("sink.".length) : "";
  const candidate = stringValue(config.format, stringValue(config.outputFormat, fromType));
  return candidate === "template" || candidate === "json" || candidate === "csv" || candidate === "xml" || candidate === "sql"
    ? candidate
    : "flat";
};

const sinkConfig = (
  node: GraphNode,
  config: Readonly<Record<string, unknown>>,
): SinkStepConfig => {
  const fields = stringList(config.fields ?? config.selectedFields);
  const format = sinkFormat(node, config);
  return {
    format,
    ...(fields.length > 0 ? { fields } : {}),
    delimiter: stringValue(config.delimiter, ", "),
    skip_empty: booleanValue(config.skip_empty ?? config.skipEmpty, true),
    unique: booleanValue(config.unique, false),
    csv_delimiter: oneCharacter(
      config.csv_delimiter ?? config.csvDelimiter ?? config.output_csv_delimiter ?? config.delimiter,
      ",",
    ),
    csv_include_header: booleanValue(
      config.csv_include_header ?? config.csvIncludeHeader ?? config.includeHeader,
      true,
    ),
    csv_quote_all: booleanValue(config.csv_quote_all ?? config.csvQuoteAll ?? config.quoteAll, false),
    xml_root: stringValue(config.xml_root, stringValue(config.xmlRoot, stringValue(config.root, "rows"))),
    xml_row: stringValue(config.xml_row, stringValue(config.xmlRow, stringValue(config.row, "row"))),
    table_name: stringValue(config.table_name, stringValue(config.tableName, stringValue(config.table, "rows"))),
    value_template: stringValue(
      config.value_template,
      stringValue(config.valueTemplate, stringValue(config.template, format === "template" ? "0x{value}" : "{value}")),
    ),
    strip_outer_quotes: booleanValue(config.strip_outer_quotes ?? config.stripOuterQuotes, true),
  };
};

const clampPreviewLimit = (value: number | undefined): number =>
  Number.isFinite(value) ? Math.max(0, Math.min(100, Math.trunc(value as number))) : 50;

/**
 * Compiles a validated Graph v2 snapshot into the stable snake_case wire shape
 * consumed by Rust/WASM. Every non-source step binds only to the connection on
 * its named `records` input; ancestors are never flattened into its config.
 */
export const buildExecutionPlanRequest = (
  graph: GraphDocument,
  options: BuildExecutionPlanOptions = {},
): ExecutePlanRequest => {
  const validation = validateGraph(graph, BUILTIN_NODE_REGISTRY);
  if (!validation.valid) throw new ExecutionPlanCompileError(validation.issues);

  const topology = stableTopologicalSort(graph);
  if (!topology.acyclic) throw new ExecutionPlanCompileError(validation.issues);

  const index = createGraphIndex(graph, BUILTIN_NODE_REGISTRY);
  const legacyById = new Map((options.legacyNodes ?? []).map((node) => [node.id, node]));
  const steps: ExecutePlanStep[] = [];
  const cacheKeysByNodeId = new Map<string, string>();

  const cacheKey = (step: Omit<ExecutePlanStepBase, "cache_key"> & {
    readonly node_type: ExecutePlanStep["node_type"];
    readonly config: object;
  }): string => computeExecutePlanStepCacheKey(
    step,
    step.input ? cacheKeysByNodeId.get(step.input.node_id) : undefined,
  );

  for (const nodeId of topology.orderedNodeIds) {
    const node = index.nodesById.get(nodeId);
    const definition = index.definitionsByNodeId.get(nodeId);
    if (!node || !definition) continue;
    const config = mergedConfig(node, legacyById.get(nodeId));

    if (definition.category === "source") {
      const base = { node_id: node.id, node_type: "source" as const, config: sourceConfig(node, config) };
      const key = cacheKey(base);
      steps.push({ ...base, cache_key: key });
      cacheKeysByNodeId.set(node.id, key);
      continue;
    }

    const inputPort = definition.ports.find((port) => port.direction === "input");
    const connection = inputPort
      ? index.incomingByEndpoint.get(endpointKey({ nodeId: node.id, port: inputPort.name }))?.[0]
      : undefined;
    if (!connection) {
      throw new ExecutionPlanCompileError([{
        code: "required_input_missing",
        severity: "error",
        message: `Обязательный вход «${inputPort?.name ?? "records"}» блока «${node.id}» не подключён`,
        nodeId: node.id,
        portName: inputPort?.name ?? "records",
      }]);
    }
    const input: ExecutePlanInput = {
      node_id: connection.from.nodeId,
      port: connection.from.port,
    };

    if (node.type === "transform.filter") {
      const base = { node_id: node.id, node_type: "filter" as const, input, config: filterConfig(node.id, config) };
      const key = cacheKey(base);
      steps.push({ ...base, cache_key: key });
      cacheKeysByNodeId.set(node.id, key);
    } else if (node.type === "transform.project") {
      const base = { node_id: node.id, node_type: "project" as const, input, config: projectConfig(config) };
      const key = cacheKey(base);
      steps.push({ ...base, cache_key: key });
      cacheKeysByNodeId.set(node.id, key);
    } else if (node.type === "transform.template") {
      const base = { node_id: node.id, node_type: "template" as const, input, config: templateConfig(config) };
      const key = cacheKey(base);
      steps.push({ ...base, cache_key: key });
      cacheKeysByNodeId.set(node.id, key);
    } else {
      const base = { node_id: node.id, node_type: "sink" as const, input, config: sinkConfig(node, config) };
      const key = cacheKey(base);
      steps.push({ ...base, cache_key: key });
      cacheKeysByNodeId.set(node.id, key);
    }
  }

  return {
    action: "execute_plan",
    plan: {
      version: 1,
      graph_id: graph.id,
      graph_revision: graph.revision,
      preview_limit: clampPreviewLimit(options.previewLimit),
      steps,
    },
  };
};

/** Ensures this module's request config remains JSON-compatible at its boundary. */
export const asJsonValue = (request: ExecutePlanRequest): JsonValue =>
  request as unknown as JsonValue;
