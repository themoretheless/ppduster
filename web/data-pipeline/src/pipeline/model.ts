import type {
  GraphConnection,
  GraphDocument,
  GraphNode,
  JsonValue,
  PortEndpoint,
} from "../graph-v2/model.ts";

export const PIPELINE_FORMAT = "peregon-pipeline" as const;
export const LEGACY_PIPELINE_FORMAT = "json-rivet-pipeline" as const;
export const PIPELINE_VERSION = 2 as const;
export const MAX_PIPELINE_BYTES = 10 * 1024 * 1024;
export const MAX_PIPELINE_NODES = 1_000;
export const MAX_PIPELINE_CONNECTIONS = 5_000;

const MAX_IDENTIFIER_LENGTH = 256;
const MAX_NODE_TYPE_LENGTH = 256;
const MAX_NAME_LENGTH = 512;
const MAX_DESCRIPTION_LENGTH = 4_096;
const MAX_TAGS = 64;
const MAX_TAG_LENGTH = 128;
const MAX_CONFIG_DEPTH = 32;
const MAX_CONFIG_VALUES = 100_000;
const MAX_CONFIG_ARRAY_LENGTH = 25_000;
const MAX_CONFIG_OBJECT_KEYS = 10_000;

const RUNTIME_ONLY_KEYS = new Set([
  "output",
  "input_schema",
  "output_schema",
  "preview",
  "preview_truncated",
  "previewStats",
  "previewError",
  "stats",
  "diagnostics",
  "sink_outputs",
  "result",
  "error",
  "executionId",
  "contentHash",
  "cache",
  "cacheKey",
  "runtime",
]);
const UNSAFE_CONFIG_KEYS = new Set(["__proto__", "prototype", "constructor"]);

export interface PipelineMetadataV2 {
  readonly name?: string;
  readonly description?: string;
  readonly tags?: readonly string[];
}

export interface PipelineViewV2 {
  readonly x: number;
  readonly y: number;
  readonly zoom: number;
  readonly selectedNodeIds?: readonly string[];
}

export interface PipelineFileV2 {
  readonly format: typeof PIPELINE_FORMAT;
  readonly version: typeof PIPELINE_VERSION;
  readonly savedAt: string;
  readonly metadata: PipelineMetadataV2;
  readonly view: PipelineViewV2;
  readonly graph: GraphDocument;
}

export type PipelineCodecIssueCode =
  | "invalid_json"
  | "file_too_large"
  | "invalid_type"
  | "invalid_value"
  | "missing_key"
  | "unknown_key"
  | "runtime_field"
  | "limit_exceeded"
  | "duplicate_id"
  | "unknown_node";

export interface PipelineCodecIssue {
  readonly path: string;
  readonly code: PipelineCodecIssueCode;
  readonly message: string;
}

export class PipelineCodecError extends Error {
  readonly issues: readonly PipelineCodecIssue[];

  constructor(issues: readonly PipelineCodecIssue[]) {
    super(issues.map((issue) => `${issue.path}: ${issue.message}`).join("; "));
    this.name = "PipelineCodecError";
    this.issues = issues;
  }
}

interface DecodeState {
  readonly issues: PipelineCodecIssue[];
  configValues: number;
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const issue = (
  state: DecodeState,
  path: string,
  code: PipelineCodecIssueCode,
  message: string,
): void => {
  state.issues.push({ path, code, message });
};

const expectRecord = (
  value: unknown,
  path: string,
  state: DecodeState,
): Record<string, unknown> | undefined => {
  if (!isRecord(value)) {
    issue(state, path, "invalid_type", "Ожидался объект");
    return undefined;
  }
  return value;
};

const checkKeys = (
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  path: string,
  state: DecodeState,
): void => {
  const allowed = new Set([...required, ...optional]);
  for (const key of required) {
    if (!Object.prototype.hasOwnProperty.call(value, key)) {
      issue(state, `${path}.${key}`, "missing_key", "Обязательное поле отсутствует");
    }
  }
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) {
      issue(
        state,
        `${path}.${key}`,
        RUNTIME_ONLY_KEYS.has(key) ? "runtime_field" : "unknown_key",
        RUNTIME_ONLY_KEYS.has(key)
          ? "Runtime-результаты и кэш не входят в файл конвейера"
          : "Неизвестное поле",
      );
    }
  }
};

const stringField = (
  value: unknown,
  path: string,
  state: DecodeState,
  options: { readonly max: number; readonly nonEmpty?: boolean },
): string => {
  if (typeof value !== "string") {
    issue(state, path, "invalid_type", "Ожидалась строка");
    return "";
  }
  if (options.nonEmpty && value.length === 0) {
    issue(state, path, "invalid_value", "Строка не должна быть пустой");
  }
  if (value.length > options.max) {
    issue(state, path, "limit_exceeded", `Строка длиннее ${options.max} символов`);
  }
  return value;
};

const finiteNumber = (
  value: unknown,
  path: string,
  state: DecodeState,
): number => {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    issue(state, path, "invalid_type", "Ожидалось конечное число");
    return 0;
  }
  return value;
};

const nonNegativeInteger = (
  value: unknown,
  path: string,
  state: DecodeState,
): number => {
  if (!Number.isSafeInteger(value) || (value as number) < 0) {
    issue(state, path, "invalid_type", "Ожидалось неотрицательное целое число");
    return 0;
  }
  return value as number;
};

const decodeMetadata = (
  value: unknown,
  state: DecodeState,
): PipelineMetadataV2 => {
  const record = expectRecord(value, "$.metadata", state) ?? {};
  checkKeys(record, [], ["name", "description", "tags"], "$.metadata", state);
  const metadata: { name?: string; description?: string; tags?: string[] } = {};
  if (record.name !== undefined) {
    metadata.name = stringField(record.name, "$.metadata.name", state, { max: MAX_NAME_LENGTH });
  }
  if (record.description !== undefined) {
    metadata.description = stringField(record.description, "$.metadata.description", state, {
      max: MAX_DESCRIPTION_LENGTH,
    });
  }
  if (record.tags !== undefined) {
    if (!Array.isArray(record.tags)) {
      issue(state, "$.metadata.tags", "invalid_type", "Ожидался массив строк");
    } else {
      if (record.tags.length > MAX_TAGS) {
        issue(state, "$.metadata.tags", "limit_exceeded", `Допустимо не больше ${MAX_TAGS} тегов`);
      }
      metadata.tags = record.tags.slice(0, MAX_TAGS).map((tag, index) =>
        stringField(tag, `$.metadata.tags[${index}]`, state, { max: MAX_TAG_LENGTH, nonEmpty: true })
      );
    }
  }
  return metadata;
};

const decodeView = (value: unknown, state: DecodeState): PipelineViewV2 => {
  const record = expectRecord(value, "$.view", state) ?? {};
  checkKeys(record, ["x", "y", "zoom"], ["selectedNodeIds"], "$.view", state);
  const zoom = finiteNumber(record.zoom, "$.view.zoom", state);
  if (zoom <= 0 || zoom > 16) {
    issue(state, "$.view.zoom", "invalid_value", "Масштаб должен быть больше 0 и не больше 16");
  }
  const view: { x: number; y: number; zoom: number; selectedNodeIds?: string[] } = {
    x: finiteNumber(record.x, "$.view.x", state),
    y: finiteNumber(record.y, "$.view.y", state),
    zoom,
  };
  if (record.selectedNodeIds !== undefined) {
    if (!Array.isArray(record.selectedNodeIds)) {
      issue(state, "$.view.selectedNodeIds", "invalid_type", "Ожидался массив id блоков");
    } else {
      if (record.selectedNodeIds.length > MAX_PIPELINE_NODES) {
        issue(
          state,
          "$.view.selectedNodeIds",
          "limit_exceeded",
          `Допустимо не больше ${MAX_PIPELINE_NODES} выбранных блоков`,
        );
      }
      const seen = new Set<string>();
      view.selectedNodeIds = record.selectedNodeIds.slice(0, MAX_PIPELINE_NODES).map((id, index) => {
        const decoded = stringField(id, `$.view.selectedNodeIds[${index}]`, state, {
          max: MAX_IDENTIFIER_LENGTH,
          nonEmpty: true,
        });
        if (seen.has(decoded)) {
          issue(
            state,
            `$.view.selectedNodeIds[${index}]`,
            "duplicate_id",
            `Блок «${decoded}» выбран повторно`,
          );
        }
        seen.add(decoded);
        return decoded;
      });
    }
  }
  return view;
};

const decodeJsonValue = (
  value: unknown,
  path: string,
  state: DecodeState,
  depth: number,
): JsonValue => {
  state.configValues += 1;
  if (state.configValues > MAX_CONFIG_VALUES) {
    issue(state, path, "limit_exceeded", `Конфигурация содержит больше ${MAX_CONFIG_VALUES} значений`);
    return null;
  }
  if (depth > MAX_CONFIG_DEPTH) {
    issue(state, path, "limit_exceeded", `Вложенность конфигурации больше ${MAX_CONFIG_DEPTH}`);
    return null;
  }
  if (value === null || typeof value === "boolean") return value;
  if (typeof value === "string") {
    if (value.length > MAX_PIPELINE_BYTES) {
      issue(state, path, "limit_exceeded", "Строка конфигурации превышает допустимый размер файла");
    }
    return value;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) issue(state, path, "invalid_value", "Число должно быть конечным");
    return Number.isFinite(value) ? value : 0;
  }
  if (Array.isArray(value)) {
    if (value.length > MAX_CONFIG_ARRAY_LENGTH) {
      issue(
        state,
        path,
        "limit_exceeded",
        `Массив конфигурации содержит больше ${MAX_CONFIG_ARRAY_LENGTH} элементов`,
      );
    }
    return value
      .slice(0, MAX_CONFIG_ARRAY_LENGTH)
      .map((item, index) => decodeJsonValue(item, `${path}[${index}]`, state, depth + 1));
  }
  if (isRecord(value)) {
    const keys = Object.keys(value);
    if (keys.length > MAX_CONFIG_OBJECT_KEYS) {
      issue(
        state,
        path,
        "limit_exceeded",
        `Объект конфигурации содержит больше ${MAX_CONFIG_OBJECT_KEYS} полей`,
      );
    }
    const output: Record<string, JsonValue> = {};
    for (const key of keys.slice(0, MAX_CONFIG_OBJECT_KEYS)) {
      if (UNSAFE_CONFIG_KEYS.has(key)) {
        issue(state, `${path}.${key}`, "invalid_value", "Небезопасное имя поля конфигурации");
        continue;
      }
      if (RUNTIME_ONLY_KEYS.has(key)) {
        issue(
          state,
          `${path}.${key}`,
          "runtime_field",
          "Runtime-результаты, preview и кэш нельзя сохранять в конфигурации",
        );
        continue;
      }
      output[key] = decodeJsonValue(value[key], `${path}.${key}`, state, depth + 1);
    }
    return output;
  }
  issue(state, path, "invalid_type", "Значение конфигурации должно быть JSON-совместимым");
  return null;
};

const decodeNode = (value: unknown, index: number, state: DecodeState): GraphNode => {
  const path = `$.graph.nodes[${index}]`;
  const record = expectRecord(value, path, state) ?? {};
  checkKeys(record, ["id", "type", "position", "config"], ["version"], path, state);
  const positionPath = `${path}.position`;
  const position = expectRecord(record.position, positionPath, state) ?? {};
  const configPath = `${path}.config`;
  const config = expectRecord(record.config, configPath, state) ?? {};
  checkKeys(position, ["x", "y"], [], positionPath, state);
  const node: GraphNode = {
    id: stringField(record.id, `${path}.id`, state, { max: MAX_IDENTIFIER_LENGTH, nonEmpty: true }),
    type: stringField(record.type, `${path}.type`, state, { max: MAX_NODE_TYPE_LENGTH, nonEmpty: true }),
    position: {
      x: finiteNumber(position.x, `${positionPath}.x`, state),
      y: finiteNumber(position.y, `${positionPath}.y`, state),
    },
    config: decodeJsonValue(config, configPath, state, 0) as Readonly<Record<string, JsonValue>>,
  };
  if (record.version !== undefined) {
    const version = nonNegativeInteger(record.version, `${path}.version`, state);
    if (version < 1) issue(state, `${path}.version`, "invalid_value", "Версия блока должна быть не меньше 1");
    return { ...node, version };
  }
  return node;
};

const decodeEndpoint = (
  value: unknown,
  path: string,
  state: DecodeState,
): PortEndpoint => {
  const record = expectRecord(value, path, state) ?? {};
  checkKeys(record, ["nodeId", "port"], [], path, state);
  return {
    nodeId: stringField(record.nodeId, `${path}.nodeId`, state, {
      max: MAX_IDENTIFIER_LENGTH,
      nonEmpty: true,
    }),
    port: stringField(record.port, `${path}.port`, state, {
      max: MAX_IDENTIFIER_LENGTH,
      nonEmpty: true,
    }),
  };
};

const decodeConnection = (
  value: unknown,
  index: number,
  state: DecodeState,
): GraphConnection => {
  const path = `$.graph.connections[${index}]`;
  const record = expectRecord(value, path, state) ?? {};
  checkKeys(record, ["id", "from", "to"], [], path, state);
  return {
    id: stringField(record.id, `${path}.id`, state, { max: MAX_IDENTIFIER_LENGTH, nonEmpty: true }),
    from: decodeEndpoint(record.from, `${path}.from`, state),
    to: decodeEndpoint(record.to, `${path}.to`, state),
  };
};

const decodeGraph = (value: unknown, state: DecodeState): GraphDocument => {
  const record = expectRecord(value, "$.graph", state) ?? {};
  checkKeys(
    record,
    ["version", "id", "revision", "nodes", "connections"],
    ["name"],
    "$.graph",
    state,
  );
  if (record.version !== 2) {
    issue(state, "$.graph.version", "invalid_value", "Поддерживается только GraphDocument версии 2");
  }
  const rawNodes = Array.isArray(record.nodes) ? record.nodes : [];
  const rawConnections = Array.isArray(record.connections) ? record.connections : [];
  if (!Array.isArray(record.nodes)) issue(state, "$.graph.nodes", "invalid_type", "Ожидался массив");
  if (!Array.isArray(record.connections)) {
    issue(state, "$.graph.connections", "invalid_type", "Ожидался массив");
  }
  if (rawNodes.length > MAX_PIPELINE_NODES) {
    issue(state, "$.graph.nodes", "limit_exceeded", `Допустимо не больше ${MAX_PIPELINE_NODES} блоков`);
  }
  if (rawConnections.length > MAX_PIPELINE_CONNECTIONS) {
    issue(
      state,
      "$.graph.connections",
      "limit_exceeded",
      `Допустимо не больше ${MAX_PIPELINE_CONNECTIONS} соединений`,
    );
  }
  const nodes = rawNodes.slice(0, MAX_PIPELINE_NODES).map((node, index) => decodeNode(node, index, state));
  const connections = rawConnections
    .slice(0, MAX_PIPELINE_CONNECTIONS)
    .map((connection, index) => decodeConnection(connection, index, state));

  const nodeIds = new Set<string>();
  nodes.forEach((node, index) => {
    if (nodeIds.has(node.id)) {
      issue(state, `$.graph.nodes[${index}].id`, "duplicate_id", `Повторяющийся id блока «${node.id}»`);
    }
    nodeIds.add(node.id);
  });
  const connectionIds = new Set<string>();
  connections.forEach((connection, index) => {
    if (connectionIds.has(connection.id)) {
      issue(
        state,
        `$.graph.connections[${index}].id`,
        "duplicate_id",
        `Повторяющийся id соединения «${connection.id}»`,
      );
    }
    connectionIds.add(connection.id);
    if (!nodeIds.has(connection.from.nodeId)) {
      issue(state, `$.graph.connections[${index}].from.nodeId`, "unknown_node", "Исходный блок не найден");
    }
    if (!nodeIds.has(connection.to.nodeId)) {
      issue(state, `$.graph.connections[${index}].to.nodeId`, "unknown_node", "Целевой блок не найден");
    }
  });

  const graph: GraphDocument = {
    version: 2,
    id: stringField(record.id, "$.graph.id", state, { max: MAX_IDENTIFIER_LENGTH, nonEmpty: true }),
    revision: nonNegativeInteger(record.revision, "$.graph.revision", state),
    nodes,
    connections,
  };
  if (record.name !== undefined) {
    return {
      ...graph,
      name: stringField(record.name, "$.graph.name", state, { max: MAX_NAME_LENGTH }),
    };
  }
  return graph;
};

const parseInput = (input: unknown, state: DecodeState): unknown => {
  if (typeof input !== "string" && !(input instanceof Uint8Array)) return input;
  const byteLength = typeof input === "string" ? new TextEncoder().encode(input).byteLength : input.byteLength;
  if (byteLength > MAX_PIPELINE_BYTES) {
    issue(state, "$", "file_too_large", `Файл больше ${MAX_PIPELINE_BYTES} байт`);
    return undefined;
  }
  const text = typeof input === "string" ? input : new TextDecoder("utf-8", { fatal: true }).decode(input);
  try {
    return JSON.parse(text) as unknown;
  } catch (error) {
    issue(
      state,
      "$",
      "invalid_json",
      error instanceof Error ? `Некорректный JSON: ${error.message}` : "Некорректный JSON",
    );
    return undefined;
  }
};

export const decodePipelineV2 = (input: unknown): PipelineFileV2 => {
  const state: DecodeState = { issues: [], configValues: 0 };
  let raw: unknown;
  try {
    raw = parseInput(input, state);
  } catch (error) {
    issue(
      state,
      "$",
      "invalid_json",
      error instanceof Error ? `Не удалось прочитать UTF-8: ${error.message}` : "Не удалось прочитать UTF-8",
    );
  }
  const record = expectRecord(raw, "$", state) ?? {};
  checkKeys(record, ["format", "version", "savedAt", "metadata", "view", "graph"], [], "$", state);
  if (record.format !== PIPELINE_FORMAT && record.format !== LEGACY_PIPELINE_FORMAT) {
    issue(state, "$.format", "invalid_value", `Ожидался формат «${PIPELINE_FORMAT}»`);
  }
  if (record.version !== PIPELINE_VERSION) {
    issue(state, "$.version", "invalid_value", `Поддерживается только версия ${PIPELINE_VERSION}`);
  }
  const savedAt = stringField(record.savedAt, "$.savedAt", state, { max: 64, nonEmpty: true });
  const savedDate = Date.parse(savedAt);
  if (!Number.isFinite(savedDate)) {
    issue(state, "$.savedAt", "invalid_value", "Дата сохранения должна быть в формате ISO 8601");
  }
  const metadata = decodeMetadata(record.metadata, state);
  const view = decodeView(record.view, state);
  const graph = decodeGraph(record.graph, state);
  const nodeIds = new Set(graph.nodes.map((node) => node.id));
  view.selectedNodeIds?.forEach((nodeId, index) => {
    if (!nodeIds.has(nodeId)) {
      issue(
        state,
        `$.view.selectedNodeIds[${index}]`,
        "unknown_node",
        `Выбранный блок «${nodeId}» отсутствует в графе`,
      );
    }
  });
  const result: PipelineFileV2 = {
    format: PIPELINE_FORMAT,
    version: PIPELINE_VERSION,
    savedAt,
    metadata,
    view,
    graph,
  };
  if (state.issues.length > 0) throw new PipelineCodecError(state.issues);
  return result;
};

export const encodePipelineV2 = (
  pipeline: PipelineFileV2,
  options: { readonly pretty?: boolean } = {},
): string => {
  const validated = decodePipelineV2(pipeline);
  const encoded = JSON.stringify(validated, null, options.pretty === false ? undefined : 2);
  if (new TextEncoder().encode(encoded).byteLength > MAX_PIPELINE_BYTES) {
    throw new PipelineCodecError([{
      path: "$",
      code: "file_too_large",
      message: `Файл больше ${MAX_PIPELINE_BYTES} байт`,
    }]);
  }
  return encoded;
};
