import {
  BUILTIN_NODE_REGISTRY,
  validateGraph,
  type GraphConnection,
  type GraphDocument,
  type GraphNode,
  type JsonValue,
} from "../graph-v2/index.ts";
import {
  legacyConditionsToExpression,
  normalizeFilterExpression,
  type FilterExpression,
} from "../runtime/filter-expression.ts";
import type {
  PlanFilterCondition,
  PlanFilterMode,
  PlanFilterOperator,
  PlanSinkFormat,
} from "../runtime/execute-plan.ts";
import {
  MAX_PIPELINE_BYTES,
  MAX_PIPELINE_CONNECTIONS,
  MAX_PIPELINE_NODES,
  LEGACY_PIPELINE_FORMAT,
  PIPELINE_FORMAT,
  type PipelineFileV2,
} from "./model.ts";

export { MAX_PIPELINE_CONNECTIONS, MAX_PIPELINE_NODES } from "./model.ts";
export const MAX_PIPELINE_SOURCE_LENGTH = MAX_PIPELINE_BYTES;

export class PipelineMigrationError extends Error {
  readonly path: string;

  constructor(message: string, path = "$") {
    super(message);
    this.name = "PipelineMigrationError";
    this.path = path;
  }
}

const FILTER_OPERATORS = new Set<PlanFilterOperator>([
  "equal", "not_equal", "greater_than", "greater_or_equal", "less_than",
  "less_or_equal", "contains", "starts_with", "ends_with", "exists", "not_exists",
]);
const SINK_FORMATS = new Set<PlanSinkFormat>(["flat", "template", "json", "csv", "xml", "sql"]);

const record = (value: unknown, path: string): Readonly<Record<string, unknown>> => {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new PipelineMigrationError("Ожидался объект", path);
  }
  return value as Readonly<Record<string, unknown>>;
};

const text = (value: unknown, fallback = "", limit = 120): string =>
  (typeof value === "string" ? value : fallback).slice(0, limit);

const finite = (value: unknown, fallback: number, min: number, max: number): number =>
  typeof value === "number" && Number.isFinite(value)
    ? Math.max(min, Math.min(max, value))
    : fallback;

const bool = (value: unknown, fallback: boolean): boolean =>
  typeof value === "boolean" ? value : fallback;

const character = (value: unknown, fallback: string): string =>
  text(value, fallback, 1) || fallback;

const normalizedId = (value: unknown, path: string, fallback?: string): string => {
  const id = text(value, fallback ?? "", 120).trim();
  if (!id) throw new PipelineMigrationError("Пустой идентификатор", path);
  return id;
};

const stringList = (value: unknown, limit = 200): string[] => {
  if (!Array.isArray(value)) return [];
  const result: string[] = [];
  const seen = new Set<string>();
  for (const candidate of value.slice(0, limit)) {
    if (typeof candidate !== "string") continue;
    const item = candidate.slice(0, 200);
    if (!seen.has(item)) {
      seen.add(item);
      result.push(item);
    }
  }
  return result;
};

const conditions = (value: unknown, path: string): PlanFilterCondition[] => {
  if (value === undefined) return [];
  if (!Array.isArray(value)) throw new PipelineMigrationError("conditions должен быть массивом", path);
  if (value.length > 100) throw new PipelineMigrationError("Слишком много условий", path);
  return value.map((candidate, index) => {
    const item = record(candidate, `${path}[${index}]`);
    if (
      typeof item.field !== "string" || !item.field.length ||
      typeof item.operator !== "string" || !FILTER_OPERATORS.has(item.operator as PlanFilterOperator)
    ) {
      throw new PipelineMigrationError("Некорректное условие фильтра", `${path}[${index}]`);
    }
    return {
      field: item.field.slice(0, 200),
      operator: item.operator as PlanFilterOperator,
      value: text(item.value, "", 10_000),
    };
  });
};

const configRecord = (node: Readonly<Record<string, unknown>>, path: string): Readonly<Record<string, unknown>> =>
  node.config === undefined ? node : { ...record(node.config, `${path}.config`), ...node };

const nodeType = (node: Readonly<Record<string, unknown>>, path: string): string => {
  if (typeof node.type === "string") return node.type;
  if (node.kind === "source") {
    if (node.sourceFormat === "csv") return "source.csv";
    if (node.sourceFormat === "list") return "source.list";
    return "source.json";
  }
  if (node.kind === "fields") return "transform.project";
  if (node.kind === "condition") return "transform.filter";
  if (node.kind === "template") return "transform.template";
  if (node.kind === "join") return "sink.join";
  if (node.kind === "output") {
    const format = SINK_FORMATS.has(node.outputFormat as PlanSinkFormat)
      ? node.outputFormat as PlanSinkFormat
      : "flat";
    return `sink.${format}`;
  }
  throw new PipelineMigrationError("Неизвестный тип блока", path);
};

const filterExpression = (
  config: Readonly<Record<string, unknown>>,
  parsedConditions: readonly PlanFilterCondition[],
  mode: PlanFilterMode,
  path: string,
): FilterExpression | null => {
  const candidate = config.expression ?? config.filterExpression ?? config.filter_ast;
  if (candidate !== undefined) {
    const normalized = normalizeFilterExpression(candidate);
    if (!normalized) throw new PipelineMigrationError("Некорректное выражение фильтра", path);
    return normalized;
  }
  return legacyConditionsToExpression(parsedConditions, mode);
};

const normalizedConfig = (
  node: Readonly<Record<string, unknown>>,
  type: string,
  path: string,
): Readonly<Record<string, JsonValue>> => {
  const config = configRecord(node, path);
  const title = text(config.title, "", 120);
  const titled: Record<string, JsonValue> = {};
  if (title) titled.title = title;
  if (type === "source.json" || type === "source.csv" || type === "source.list") {
    const source = text(config.data ?? config.json ?? config.text, "", MAX_PIPELINE_SOURCE_LENGTH);
    return {
      ...titled,
      text: source,
      arrayPath: text(config.path ?? config.arrayPath ?? config.selectedPath, "", 2_000),
      ...(type === "source.csv" ? {
        delimiter: character(config.csv_delimiter ?? config.csvDelimiter ?? config.delimiter, ","),
        includeHeader: bool(config.includeHeader ?? config.csvIncludeHeader, true),
      } : {}),
    };
  }
  if (type === "transform.project") {
    return {
      ...titled,
      fields: stringList(config.fields ?? config.selectedFields),
      // Retained temporarily so v1 array selection can be promoted to its source below.
      ...(typeof config.selectedPath === "string" ? { selectedPath: config.selectedPath.slice(0, 2_000) } : {}),
    };
  }
  if (type === "transform.filter") {
    const parsedConditions = conditions(config.filters ?? config.conditions, `${path}.conditions`);
    const mode: PlanFilterMode =
      config.filter_mode === "any" || config.mode === "any" || config.filterMode === "any"
        ? "any"
        : "all";
    const expression = filterExpression(config, parsedConditions, mode, `${path}.filterExpression`);
    return {
      ...titled,
      mode,
      conditions: parsedConditions as unknown as JsonValue,
      ...(expression ? { expression: expression as unknown as JsonValue } : {}),
    };
  }
  if (type === "transform.template") {
    return {
      ...titled,
      template: text(config.value_template ?? config.valueTemplate ?? config.template, "0x{value}", 2_000),
      stripOuterQuotes: bool(config.strip_outer_quotes ?? config.stripOuterQuotes, true),
      skipEmpty: bool(config.skip_empty ?? config.skipEmpty, true),
    };
  }
  if (type.startsWith("sink.")) {
    const format = type.slice(5) as PlanSinkFormat;
    return {
      ...titled,
      format,
      delimiter: text(config.delimiter, ", ", 12),
      skipEmpty: bool(config.skip_empty ?? config.skipEmpty, true),
      unique: bool(config.unique, false),
      csvDelimiter: character(
        config.csv_delimiter ?? config.csvDelimiter ?? (format === "csv" ? config.delimiter : undefined),
        ",",
      ),
      csvIncludeHeader: bool(config.csv_include_header ?? config.csvIncludeHeader ?? config.includeHeader, true),
      csvQuoteAll: bool(config.csv_quote_all ?? config.csvQuoteAll ?? config.quoteAll, false),
      xmlRoot: text(config.xml_root ?? config.xmlRoot ?? config.root, "rows", 64),
      xmlRow: text(config.xml_row ?? config.xmlRow ?? config.row, "row", 64),
      tableName: text(config.table_name ?? config.tableName ?? config.table, "result", 64),
      valueTemplate: text(
        config.value_template ?? config.valueTemplate ?? config.template,
        format === "template" ? "0x{value}" : "{value}",
        2_000,
      ),
      stripOuterQuotes: bool(config.strip_outer_quotes ?? config.stripOuterQuotes, true),
      ...(stringList(config.fields ?? config.selectedFields).length
        ? { fields: stringList(config.fields ?? config.selectedFields) }
        : {}),
    };
  }
  throw new PipelineMigrationError("Неподдерживаемый тип блока", path);
};

const outputPort = (type: string): string => type === "source.list"
  ? "values"
  : type === "transform.template" ? "mapped"
  : type.startsWith("source.") ? "records" : "matched";

const inputPort = (type: string): string =>
  type === "sink.template" || type === "sink.join" || type === "transform.template" ? "values" : "records";

const metadata = (raw: Readonly<Record<string, unknown>>): PipelineFileV2["metadata"] => {
  const source = raw.metadata && typeof raw.metadata === "object" && !Array.isArray(raw.metadata)
    ? raw.metadata as Readonly<Record<string, unknown>>
    : raw;
  const name = text(source.name, "", 120);
  const description = text(source.description, "", 2_000);
  const tags = stringList(source.tags, 32).map((tag) => tag.slice(0, 64));
  return {
    ...(name ? { name } : {}),
    ...(description ? { description } : {}),
    ...(tags.length ? { tags } : {}),
  };
};

const savedAt = (value: unknown): string => {
  const candidate = text(value, "", 64);
  return candidate && Number.isFinite(Date.parse(candidate))
    ? candidate
    : "1970-01-01T00:00:00.000Z";
};

const view = (raw: Readonly<Record<string, unknown>>, ids: ReadonlySet<string>): PipelineFileV2["view"] => {
  const source = raw.view && typeof raw.view === "object" && !Array.isArray(raw.view)
    ? raw.view as Readonly<Record<string, unknown>>
    : {};
  const selected = stringList(source.selectedNodeIds, MAX_PIPELINE_NODES).filter((id) => ids.has(id));
  return {
    x: finite(source.x ?? source.panX, 0, -1_000_000, 1_000_000),
    y: finite(source.y ?? source.panY, 0, -1_000_000, 1_000_000),
    zoom: finite(source.zoom, 0.8, 0.42, 1.65),
    ...(selected.length ? { selectedNodeIds: selected } : {}),
  };
};

const promoteLegacyArrayPaths = (
  nodes: readonly GraphNode[],
  connections: readonly GraphConnection[],
): GraphNode[] => {
  const pathsBySource = new Map<string, Set<string>>();
  const nodesById = new Map(nodes.map((node) => [node.id, node]));
  for (const connection of connections) {
    const source = nodesById.get(connection.from.nodeId);
    const target = nodesById.get(connection.to.nodeId);
    const selectedPath = target?.type === "transform.project" && typeof target.config.selectedPath === "string"
      ? target.config.selectedPath
      : "";
    if (source?.type.startsWith("source.") && selectedPath) {
      const paths = pathsBySource.get(source.id) ?? new Set<string>();
      paths.add(selectedPath);
      pathsBySource.set(source.id, paths);
    }
  }
  return nodes.map((node) => {
    const { selectedPath: _selectedPath, ...withoutLegacyPath } = node.config;
    if (!node.type.startsWith("source.")) return { ...node, config: withoutLegacyPath };
    const paths = pathsBySource.get(node.id);
    if (!paths?.size || typeof node.config.arrayPath === "string" && node.config.arrayPath) return node;
    if (paths.size > 1) {
      throw new PipelineMigrationError("Один источник использует несколько разных путей массива", `$.graph.nodes.${node.id}`);
    }
    return { ...node, config: { ...node.config, arrayPath: [...paths][0] } };
  });
};

export const migratePipelineFile = (value: unknown): PipelineFileV2 => {
  const raw = record(value, "$");
  if (raw.format !== PIPELINE_FORMAT && raw.format !== LEGACY_PIPELINE_FORMAT) {
    throw new PipelineMigrationError("Неподдерживаемый формат файла");
  }
  if (raw.version !== 1 && raw.version !== 2) throw new PipelineMigrationError("Неподдерживаемая версия файла", "$.version");
  const graphSource = raw.version === 2 ? record(raw.graph, "$.graph") : raw;
  if (!Array.isArray(graphSource.nodes)) throw new PipelineMigrationError("В файле нет блоков", "$.nodes");
  const rawConnections = raw.version === 2 ? graphSource.connections : raw.edges;
  if (!Array.isArray(rawConnections)) throw new PipelineMigrationError("В файле нет связей", "$.edges");
  if (graphSource.nodes.length > MAX_PIPELINE_NODES || rawConnections.length > MAX_PIPELINE_CONNECTIONS) {
    throw new PipelineMigrationError("Схема слишком большая");
  }

  const ids = new Set<string>();
  const nodes = graphSource.nodes.map((candidate, index): GraphNode => {
    const path = `$.nodes[${index}]`;
    const rawNode = record(candidate, path);
    const id = normalizedId(rawNode.id, `${path}.id`, `node-${index + 1}`);
    if (ids.has(id)) throw new PipelineMigrationError("Повторяющийся идентификатор блока", `${path}.id`);
    ids.add(id);
    const type = nodeType(rawNode, path);
    if (!BUILTIN_NODE_REGISTRY.has(type)) throw new PipelineMigrationError("Неподдерживаемый тип блока", `${path}.type`);
    const position = rawNode.position && typeof rawNode.position === "object" && !Array.isArray(rawNode.position)
      ? rawNode.position as Readonly<Record<string, unknown>>
      : rawNode;
    return {
      id,
      type,
      version: 1,
      position: {
        x: finite(position.x, index * 390, -1_000_000, 1_000_000),
        y: finite(position.y, 100, -1_000_000, 1_000_000),
      },
      config: normalizedConfig(rawNode, type, path),
    };
  });
  const typeById = new Map(nodes.map((node) => [node.id, node.type]));
  const connectionIds = new Set<string>();
  const connections = rawConnections.map((candidate, index): GraphConnection => {
    const path = `$.connections[${index}]`;
    const rawConnection = record(candidate, path);
    const fromObject = rawConnection.from && typeof rawConnection.from === "object"
      ? record(rawConnection.from, `${path}.from`)
      : undefined;
    const toObject = rawConnection.to && typeof rawConnection.to === "object"
      ? record(rawConnection.to, `${path}.to`)
      : undefined;
    const fromNodeId = normalizedId(fromObject?.nodeId ?? rawConnection.from, `${path}.from`);
    const toNodeId = normalizedId(toObject?.nodeId ?? rawConnection.to, `${path}.to`);
    if (!ids.has(fromNodeId) || !ids.has(toNodeId) || fromNodeId === toNodeId) {
      throw new PipelineMigrationError("Некорректные концы связи", path);
    }
    const id = normalizedId(rawConnection.id, `${path}.id`, `connection-${index + 1}`);
    if (connectionIds.has(id)) throw new PipelineMigrationError("Повторяющийся идентификатор связи", `${path}.id`);
    connectionIds.add(id);
    return {
      id,
      from: {
        nodeId: fromNodeId,
        port: outputPort(typeById.get(fromNodeId) ?? "") === "values"
          ? outputPort(typeById.get(fromNodeId) ?? "")
          : text(fromObject?.port, outputPort(typeById.get(fromNodeId) ?? ""), 120),
      },
      to: {
        nodeId: toNodeId,
        port: inputPort(typeById.get(toNodeId) ?? "") === "values"
          ? inputPort(typeById.get(toNodeId) ?? "")
          : text(toObject?.port, "records", 120),
      },
    };
  });

  const normalizedNodes = promoteLegacyArrayPaths(nodes, connections);
  const graph: GraphDocument = {
    version: 2,
    id: normalizedId(graphSource.id ?? raw.id, "$.graph.id", "pipeline"),
    ...(typeof graphSource.name === "string" ? { name: graphSource.name.slice(0, 120) } : {}),
    revision: Math.max(0, Math.trunc(finite(graphSource.revision, 1, 0, Number.MAX_SAFE_INTEGER))),
    nodes: normalizedNodes,
    connections,
  };
  const validation = validateGraph(graph);
  if (!validation.valid) {
    throw new PipelineMigrationError(validation.issues[0]?.message ?? "Некорректный граф", "$.graph");
  }
  return {
    format: PIPELINE_FORMAT,
    version: 2,
    savedAt: savedAt(raw.savedAt),
    metadata: metadata(raw),
    view: view(raw, ids),
    graph,
  };
};
