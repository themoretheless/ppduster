<script setup lang="ts">
import {
  computed,
  nextTick,
  onBeforeUnmount,
  onMounted,
  reactive,
  ref,
  watch,
} from "vue";
import { JsonEngineClient } from "./engine/client";
import FilterExpressionEditor from "./components/FilterExpressionEditor.vue";
import JsonCodeEditor from "./components/JsonCodeEditor.vue";
import PlainTextViewer from "./components/PlainTextViewer.vue";
import {
  BUILTIN_NODE_REGISTRY,
  arePortContractsCompatible,
  preflightConnection,
  validateGraph,
  type GraphDocument,
  type GraphNode,
} from "./graph-v2";
import {
  buildExecutionPlanRequest,
  type ExecutePlanNodeResult,
  type ExecutePlanResponse,
} from "./runtime/execute-plan.ts";
import { inputFieldsForNode, outputFieldsForNode } from "./runtime/schema-state.ts";
import {
  createUiFilterGroup,
  filterExpressionToUi,
  legacyConditionsToUiExpression,
  type UiFilterExpression,
} from "./runtime/filter-ui.ts";
import { normalizeFilterExpression } from "./runtime/filter-expression.ts";
import { PIPELINE_FORMAT, decodePipelineV2, encodePipelineV2, type PipelineFileV2 } from "./pipeline/model.ts";
import { migratePipelineFile } from "./pipeline/migrate.ts";
import type {
  AnalyzeResponse,
  AnalyzeSuccess,
  FilterCondition,
  FilterMode,
  FilterOperator,
  OutputFormat,
  SourceFormat,
  TimedResponse,
  TransformResponse,
  TransformSuccess,
} from "./engine/types";
import { FlowSurfaceRenderer, type SurfaceEdge } from "./graph/webgpu";

type NodeKind = "source" | "fields" | "condition" | "template" | "join" | "output";
type PortDirection = "input" | "output";
type Theme = "light" | "dark";

interface UiCondition extends FilterCondition {
  id: number;
}

interface FlowNode {
  id: string;
  kind: NodeKind;
  title: string;
  x: number;
  y: number;
  json?: string;
  sourceFormat?: SourceFormat;
  csvDelimiter?: string;
  csvIncludeHeader?: boolean;
  csvQuoteAll?: boolean;
  selectedPath?: string;
  selectedFields?: string[];
  conditions?: UiCondition[];
  filterMode?: FilterMode;
  filterExpression?: UiFilterExpression;
  delimiter?: string;
  outputFormat?: OutputFormat;
  xmlRoot?: string;
  xmlRow?: string;
  tableName?: string;
  valueTemplate?: string;
  stripOuterQuotes?: boolean;
  output?: string;
  stats?: TransformSuccess | null;
  preview?: string;
  previewStats?: TransformSuccess | null;
  previewError?: string;
  error?: string;
}

interface FlowEdge {
  id: string;
  from: string;
  to: string;
}

interface EdgeVisual extends FlowEdge {
  path: string;
  midX: number;
  midY: number;
}

interface CacheEntry<T> {
  signature: string;
  response: T;
  engineVersion: string;
}

interface BlockDefinition {
  key: string;
  kind: NodeKind;
  format?: SourceFormat;
  outputFormat?: OutputFormat;
  label: string;
  eyebrow: string;
  icon: string;
  color: string;
  keywords: string;
}

interface LoadedPipeline {
  title?: string;
  view: { panX: number; panY: number; zoom: number };
  nodes: FlowNode[];
  edges: FlowEdge[];
}

const PIPELINE_STORAGE_KEY = "peregon-pipeline-state-v1";

interface InspectorPane {
  content: string;
  language: string;
  emptyMessage: string;
  itemCount?: number;
  truncated?: boolean;
  error?: string;
}

const SAMPLE_JSON = `{
  "stores": [
    {
      "id": "A-101",
      "name": "Москва 4-10",
      "locality": "Москва",
      "state": 1
    },
    {
      "id": "B-204",
      "name": "Белгород-2",
      "locality": "Белгород",
      "state": 1
    },
    {
      "id": "K-307",
      "name": "Курск-1",
      "locality": "Курск",
      "state": 0
    }
  ]
}`;

const SAMPLE_CSV = `id,name,locality,state
A-101,Москва 4-10,Москва,1
B-204,Белгород-2,Белгород,1
K-307,Курск-1,Курск,0`;

const SAMPLE_LIST = `"000D3A23B0DC80D811E68F98E099F565",
"000D3A22FA54A81611EB145B4F1E2FEB",
"000D3A21DA51A81211E9DC4536C21386"`;

const NODE_META: Record<NodeKind, { label: string; eyebrow: string; icon: string; color: string }> = {
  source: { label: "Данные", eyebrow: "Источник", icon: "{ }", color: "#6557d9" },
  fields: { label: "Поля", eyebrow: "Проекция", icon: "⌗", color: "#367fbb" },
  condition: { label: "Условие", eyebrow: "Фильтрация", icon: "ƒ", color: "#159288" },
  template: { label: "По шаблону", eyebrow: "Преобразование", icon: "{x}", color: "#d06a35" },
  join: { label: "Объединить", eyebrow: "Сериализация", icon: "→", color: "#d06a35" },
  output: { label: "Результат", eyebrow: "Выход", icon: "→", color: "#d06a35" },
};

const VERSION_INFO = __PEREGON_VERSION_INFO__;
const VERSION_GROUPS = [
  {
    label: "Приложение",
    packages: [VERSION_INFO.project, VERSION_INFO.engine],
  },
  {
    label: "Runtime · npm",
    packages: VERSION_INFO.packages.npmRuntime,
  },
  {
    label: "Runtime · Rust",
    packages: VERSION_INFO.packages.rustRuntime,
  },
  {
    label: "Сборка",
    packages: VERSION_INFO.packages.build,
  },
] as const;

const LIBRARY_BLOCKS: BlockDefinition[] = [
  { key: "json", kind: "source", format: "json", label: "JSON", eyebrow: "Источник", icon: "{ }", color: "#6557d9", keywords: "json данные источник" },
  { key: "csv", kind: "source", format: "csv", label: "CSV", eyebrow: "Источник", icon: "CSV", color: "#8b5fbf", keywords: "csv таблица данные источник" },
  { key: "list", kind: "source", format: "list", label: "Список", eyebrow: "Источник", icon: "≡", color: "#8b5fbf", keywords: "список строки текст без json значения" },
  { key: "fields", kind: "fields", label: "Поля", eyebrow: "Проекция", icon: "⌗", color: "#367fbb", keywords: "поля выбрать оставить проекция" },
  { key: "condition", kind: "condition", label: "Условие", eyebrow: "Фильтрация", icon: "ƒ", color: "#159288", keywords: "условие фильтр отбор where" },
  { key: "flat", kind: "output", outputFormat: "flat", label: "Плоский список", eyebrow: "Выход", icon: "→", color: "#d06a35", keywords: "поля текст строка список через запятую" },
  { key: "join", kind: "join", label: "Объединить", eyebrow: "Сериализация", icon: "→", color: "#d06a35", keywords: "текст строка список склеить разделитель через запятую" },
  { key: "template", kind: "template", label: "По шаблону", eyebrow: "Преобразование", icon: "{x}", color: "#d06a35", keywords: "шаблон префикс суффикс 0x преобразовать перегон map" },
  { key: "json-output", kind: "output", outputFormat: "json", label: "JSON", eyebrow: "Выход", icon: "{ }", color: "#d06a35", keywords: "json результат экспорт выход" },
  { key: "csv-output", kind: "output", outputFormat: "csv", label: "CSV конвертер", eyebrow: "Конвертация", icon: "CSV", color: "#d06a35", keywords: "json в csv конвертация таблица результат экспорт выход" },
  { key: "xml-output", kind: "output", outputFormat: "xml", label: "XML конвертер", eyebrow: "Конвертация", icon: "XML", color: "#b75a73", keywords: "json в xml конвертация разметка результат экспорт выход" },
  { key: "sql-output", kind: "output", outputFormat: "sql", label: "SQL INSERT", eyebrow: "Выход", icon: "SQL", color: "#d06a35", keywords: "sql insert база запрос результат экспорт" },
];

const FILTER_OPERATORS: Array<{ value: FilterOperator; label: string }> = [
  { value: "equal", label: "равно" },
  { value: "not_equal", label: "не равно" },
  { value: "greater_than", label: "больше" },
  { value: "greater_or_equal", label: "не меньше" },
  { value: "less_than", label: "меньше" },
  { value: "less_or_equal", label: "не больше" },
  { value: "contains", label: "содержит" },
  { value: "starts_with", label: "начинается с" },
  { value: "ends_with", label: "заканчивается на" },
  { value: "exists", label: "существует" },
  { value: "not_exists", label: "отсутствует" },
];

function createPresetPipeline(title: string, sourceFormat: SourceFormat, sourceText: string, outputFormat: OutputFormat): LoadedPipeline {
  return {
    title,
    view: { panX: 0, panY: 0, zoom: 0.8 },
    nodes: [
      {
        id: "source-1",
        kind: "source",
        title: sourceFormat === "csv" ? "CSV-данные" : sourceFormat === "list" ? "Список" : "Данные магазинов",
        x: 80,
        y: 190,
        json: sourceText,
        sourceFormat,
        csvDelimiter: ",",
      },
      {
        id: "fields-1",
        kind: "fields",
        title: "Выбрать поля",
        x: 865,
        y: 135,
        selectedPath: "/stores",
        selectedFields: ["name"],
      },
      {
        id: "condition-1",
        kind: "condition",
        title: "Только активные",
        x: 480,
        y: 115,
        filterMode: "all",
        conditions: [{ id: 1, field: "state", operator: "equal", value: "1" }],
        filterExpression: createUiFilterGroup("and", "state"),
      },
      {
        id: "output-1",
        kind: "output",
        title: outputFormat === "json" ? "JSON результат" : "Плоский список",
        x: 1280,
        y: 180,
        delimiter: ", ",
        csvDelimiter: ",",
        csvIncludeHeader: true,
        csvQuoteAll: false,
        outputFormat,
        xmlRoot: "rows",
        xmlRow: "row",
        tableName: "stores",
        output: "",
        stats: null,
      },
    ],
    edges: [
      { id: "edge-1", from: "source-1", to: "condition-1" },
      { id: "edge-2", from: "condition-1", to: "fields-1" },
      { id: "edge-3", from: "fields-1", to: "output-1" },
    ],
  };
}

function createEmptyPipeline(title: string): LoadedPipeline {
  return {
    title,
    view: { panX: 0, panY: 0, zoom: 0.8 },
    nodes: [{
      id: "source-1",
      kind: "source",
      title: "Новые данные",
      x: 80,
      y: 190,
      json: "[]",
      sourceFormat: "json",
      csvDelimiter: ",",
    }],
    edges: [],
  };
}

const PIPELINE_PRESETS = [
  {
    id: "stores",
    title: "Магазины",
    description: "JSON + фильтр + плоский список",
    build: () => createPresetPipeline("Обработка магазинов", "json", SAMPLE_JSON, "flat"),
  },
  {
    id: "csv-active",
    title: "CSV активных",
    description: "CSV + поля + JSON-выход",
    build: () => createPresetPipeline("CSV активных", "csv", SAMPLE_CSV, "json"),
  },
  {
    id: "blank",
    title: "Пустой холст",
    description: "Начать с нуля",
    build: () => createEmptyPipeline("Пустой холст"),
  },
];

const initialPipeline = createPresetPipeline("Обработка магазинов", "json", SAMPLE_JSON, "flat");
const nodes = ref<FlowNode[]>(initialPipeline.nodes);
const edges = ref<FlowEdge[]>(initialPipeline.edges);
const pipelineTitle = ref(initialPipeline.title ?? "Обработка магазинов");

const analyses = reactive<Record<string, AnalyzeSuccess | undefined>>({});
const executionResults = reactive<Record<string, ExecutePlanNodeResult | undefined>>({});
const executionResponse = ref<ExecutePlanResponse | null>(null);
const board = ref<HTMLElement | null>(null);
const surface = ref<HTMLCanvasElement | null>(null);
const panX = ref(0);
const panY = ref(0);
const zoom = ref(0.8);
const selectedNodeId = ref("");
const inspectedNodeId = ref("");
const connectingFrom = ref("");
const connectionPreview = ref<{ x: number; y: number } | null>(null);
const edgeVisuals = ref<EdgeVisual[]>([]);
const gpuMode = ref<"loading" | "webgpu" | "canvas">("loading");
const engineVersion = ref("");
const isRunning = ref(false);
const lastRunMs = ref(0);
const cachedBranches = ref(0);
const notice = ref("Перетащите блок или соедините точки");
const continuationFor = ref("");
const continuationQuery = ref("");
const pipelineFileInput = ref<HTMLInputElement | null>(null);
const theme = ref<Theme>("light");

let client: JsonEngineClient | null = null;
let renderer: FlowSurfaceRenderer | null = null;
let resizeObserver: ResizeObserver | null = null;
let renderFrame = 0;
let executeTimer: number | undefined;
let executionToken = 0;
let nextNodeId = 2;
let nextEdgeId = 4;
let nextConditionId = 2;
let noticeTimer: number | undefined;
let connectionDrag: { fromId: string; startX: number; startY: number; moved: boolean } | null = null;
const analysisCache = new Map<string, CacheEntry<AnalyzeResponse>>();
const outputCache = new Map<string, CacheEntry<TransformResponse>>();
const previewCache = new Map<string, CacheEntry<TransformResponse>>();

type Gesture =
  | { type: "pan"; startX: number; startY: number; originX: number; originY: number }
  | { type: "node"; nodeId: string; startX: number; startY: number; originX: number; originY: number; moved: boolean }
  | null;
let gesture: Gesture = null;
let suppressNodeClickId = "";
let suppressNodeClickTimer: number | undefined;

const zoomLabel = computed(() => `${Math.round(zoom.value * 100)}%`);
const executionSignature = computed(() =>
  JSON.stringify({
    edges: edges.value.map(({ from, to }) => [from, to]),
    nodes: nodes.value.map((node) => ({
      id: node.id,
      kind: node.kind,
      json: node.json,
      sourceFormat: node.sourceFormat,
      csvDelimiter: node.csvDelimiter,
      csvIncludeHeader: node.csvIncludeHeader,
      csvQuoteAll: node.csvQuoteAll,
      selectedPath: node.selectedPath,
      selectedFields: node.selectedFields,
      conditions: node.conditions,
      filterMode: node.filterMode,
      filterExpression: node.filterExpression,
      delimiter: node.delimiter,
      outputFormat: node.outputFormat,
      xmlRoot: node.xmlRoot,
      xmlRow: node.xmlRow,
      tableName: node.tableName,
      valueTemplate: node.valueTemplate,
      stripOuterQuotes: node.stripOuterQuotes,
    })),
  }),
);
const geometrySignature = computed(() =>
  JSON.stringify({
    panX: panX.value,
    panY: panY.value,
    zoom: zoom.value,
    nodes: nodes.value.map(({ id, x, y }) => [id, x, y]),
    edges: edges.value,
  }),
);
const persistSignature = computed(() => JSON.stringify({
  title: pipelineTitle.value,
  execution: executionSignature.value,
  geometry: geometrySignature.value,
}));

function applyTheme(nextTheme: Theme) {
  theme.value = nextTheme;
  document.documentElement.dataset.theme = nextTheme;
  document.documentElement.style.colorScheme = nextTheme;
  document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')
    ?.setAttribute("content", nextTheme === "dark" ? "#0e1114" : "#f3f4ef");
  localStorage.setItem("peregon-theme", nextTheme);
  scheduleRender();
}

function toggleTheme() {
  applyTheme(theme.value === "dark" ? "light" : "dark");
}

function nodeMeta(kind: NodeKind) {
  return NODE_META[kind];
}

function nodeDisplayMeta(node: FlowNode) {
  if (node.kind !== "source") return nodeMeta(node.kind);
  if (node.sourceFormat === "csv") {
    return { label: "CSV", eyebrow: "Источник", icon: "CSV", color: "#8b5fbf" };
  }
  if (node.sourceFormat === "list") {
    return { label: "Список", eyebrow: "Источник", icon: "≡", color: "#8b5fbf" };
  }
  return { label: "JSON", eyebrow: "Источник", icon: "{ }", color: "#6557d9" };
}

function nodeById(id: string) {
  return nodes.value.find((node) => node.id === id);
}

const inspectedNode = computed(() => nodeById(inspectedNodeId.value));

function previewContent(result: ExecutePlanNodeResult | undefined) {
  return result ? JSON.stringify(result.preview, null, 2) : "";
}

function listPreviewContent(result: ExecutePlanNodeResult | undefined) {
  if (!result) return "";
  return result.preview.flatMap((item) => {
    if (typeof item === "string") return [item];
    if (typeof item !== "object" || item === null || Array.isArray(item)) return [];
    const value = (item as Readonly<Record<string, unknown>>).value;
    return typeof value === "string" ? [value] : [];
  }).join("\n");
}

function resultContentForNode(node: FlowNode | undefined, result: ExecutePlanNodeResult | undefined) {
  return node?.kind === "template" || node?.kind === "source" && node.sourceFormat === "list"
    ? listPreviewContent(result)
    : previewContent(result);
}

function resultError(result: ExecutePlanNodeResult | undefined) {
  return result && !result.ok
    ? result.diagnostics[0]?.message ?? "Блок не был выполнен"
    : "";
}

function clearExecutionResults() {
  executionResponse.value = null;
  for (const nodeId of Object.keys(executionResults)) delete executionResults[nodeId];
}

function inspectorInputFor(node: FlowNode): InspectorPane {
  if (node.kind === "source") {
    return {
      content: node.json ?? "",
      language: node.sourceFormat ?? "json",
      emptyMessage: "Исходные данные пусты",
    };
  }

  const edge = incomingEdges(node.id)[0];
  if (!edge) {
    return {
      content: "",
      language: "json",
      emptyMessage: "Вход не подключён",
    };
  }

  const upstreamResult = executionResults[edge.from];
  const upstreamNode = nodeById(edge.from);
  const ownResult = executionResults[node.id];
  return {
    content: upstreamResult?.ok ? resultContentForNode(upstreamNode, upstreamResult) : "",
    language: upstreamNode?.kind === "template" || upstreamNode?.kind === "source" && upstreamNode.sourceFormat === "list" ? "list" : "json",
    emptyMessage: upstreamResult
      ? "На входе нет строк"
      : isRunning.value ? "Вычисляю вход…" : "Запустите граф, чтобы увидеть вход",
    itemCount: ownResult?.stats.input_items ?? upstreamResult?.stats.output_items,
    truncated: upstreamResult?.preview_truncated,
    error: resultError(upstreamResult),
  };
}

function inspectorOutputFor(node: FlowNode): InspectorPane {
  const result = executionResults[node.id];
  const error = resultError(result) || node.error || node.previewError || "";
  if (node.kind === "output" || node.kind === "join") {
    return {
      content: result?.ok ? node.output ?? "" : "",
      language: node.kind === "join" ? "flat" : node.outputFormat ?? "flat",
      emptyMessage: result?.ok
        ? "Результат пуст"
        : isRunning.value ? "Вычисляю результат…" : "Запустите граф, чтобы увидеть результат",
      itemCount: result?.stats.output_items,
      error,
    };
  }

  return {
    content: result?.ok ? resultContentForNode(node, result) : "",
    language: node.kind === "template" || node.kind === "source" && node.sourceFormat === "list" ? "list" : "json",
    emptyMessage: result?.ok
      ? "На выходе нет строк"
      : isRunning.value ? "Вычисляю результат…" : "Запустите граф, чтобы увидеть результат",
    itemCount: result?.stats.output_items,
    truncated: result?.preview_truncated,
    error,
  };
}

const inspectorInput = computed(() => inspectedNode.value
  ? inspectorInputFor(inspectedNode.value)
  : null);
const inspectorOutput = computed(() => inspectedNode.value
  ? inspectorOutputFor(inspectedNode.value)
  : null);

function inspectorHighlights(language: string) {
  return language === "json" || language === "xml" || language === "sql";
}

function openNodeInspector(node: FlowNode) {
  selectedNodeId.value = node.id;
  inspectedNodeId.value = node.id;
  closeContinuation();
}

function closeNodeInspector() {
  inspectedNodeId.value = "";
}

function handleNodeCardClick(event: MouseEvent, node: FlowNode) {
  if (suppressNodeClickId === node.id) return;
  const target = event.target;
  if (target instanceof Element) {
    if (target.closest(
      "button, input, select, a, label, [contenteditable='true'], .continuation-popover",
    )) return;
    const textarea = target.closest("textarea");
    if (textarea && !textarea.readOnly) return;
  }
  openNodeInspector(node);
}

function graphV2Type(node: FlowNode): string {
  if (node.kind === "source") return `source.${node.sourceFormat ?? "json"}`;
  if (node.kind === "fields") return "transform.project";
  if (node.kind === "condition") return "transform.filter";
  if (node.kind === "template") return "transform.template";
  if (node.kind === "join") return "sink.join";
  return `sink.${node.outputFormat ?? "flat"}`;
}

function graphV2Port(node: FlowNode, direction: PortDirection) {
  return BUILTIN_NODE_REGISTRY.get(graphV2Type(node))?.ports.find((port) => port.direction === direction);
}

function graphV2OutputPort(node: FlowNode): string {
  return graphV2Port(node, "output")?.name ?? "matched";
}

function graphV2InputPort(node: FlowNode): string {
  return graphV2Port(node, "input")?.name ?? "records";
}

function hasInputPort(node: FlowNode) {
  return Boolean(graphV2Port(node, "input"));
}

function hasOutputPort(node: FlowNode) {
  return Boolean(graphV2Port(node, "output"));
}

function toGraphV2(graphNodes = nodes.value, graphEdges = edges.value): GraphDocument {
  return {
    version: 2,
    id: "current-pipeline",
    revision: 1,
    nodes: graphNodes.map((node): GraphNode => ({
      id: node.id,
      type: graphV2Type(node),
      version: 1,
      position: { x: node.x, y: node.y },
      config: {},
    })),
    connections: graphEdges.map((edge) => {
      const source = graphNodes.find((node) => node.id === edge.from);
      const target = graphNodes.find((node) => node.id === edge.to);
      return {
        id: edge.id,
        from: { nodeId: edge.from, port: source ? graphV2OutputPort(source) : "records" },
        to: {
          nodeId: edge.to,
          port: target ? graphV2InputPort(target) : "records",
        },
      };
    }),
  };
}

function incomingEdges(nodeId: string) {
  return edges.value.filter((edge) => edge.to === nodeId);
}

function outgoingEdges(nodeId: string) {
  return edges.value.filter((edge) => edge.from === nodeId);
}

function collectAncestors(nodeId: string, visited = new Set<string>()): FlowNode[] {
  const result: FlowNode[] = [];
  for (const edge of incomingEdges(nodeId)) {
    if (visited.has(edge.from)) continue;
    visited.add(edge.from);
    const parent = nodeById(edge.from);
    if (!parent) continue;
    result.push(parent, ...collectAncestors(parent.id, visited));
  }
  return result;
}

function upstreamSource(nodeId: string) {
  return collectAncestors(nodeId).find((node) => node.kind === "source");
}

function analysisForNode(nodeId: string) {
  const source = upstreamSource(nodeId);
  return source ? analyses[source.id] : undefined;
}

function arraysForNode(nodeId: string) {
  return analysisForNode(nodeId)?.array_paths ?? [];
}

function recordFields(schema: ExecutePlanNodeResult["input_schema"] | undefined) {
  return schema?.kind === "records" ? schema.fields : undefined;
}

function fieldsForNode(node: FlowNode) {
  const runtimeFields = recordFields(executionResults[node.id]?.input_schema);
  if (runtimeFields) return runtimeFields;
  const analysis = analysisForNode(node.id);
  const arrays = analysis?.array_paths ?? [];
  const selected = arrays.find((candidate) => candidate.path === node.selectedPath) ?? arrays[0];
  return selected?.fields ?? [];
}

function availableConditionFields(nodeId: string) {
  const runtimeFields = recordFields(executionResults[nodeId]?.input_schema);
  if (runtimeFields) return runtimeFields;
  const ancestor = collectAncestors(nodeId).find((node) => node.kind === "fields");
  if (ancestor) return fieldsForNode(ancestor);
  const analysis = analysisForNode(nodeId);
  return analysis?.array_paths[0]?.fields ?? [];
}

function fieldCount(count: number) {
  const word = count % 10 === 1 && count % 100 !== 11
    ? "поле"
    : [2, 3, 4].includes(count % 10) && ![12, 13, 14].includes(count % 100)
      ? "поля"
      : "полей";
  return `${count} ${word}`;
}

function nodeSchemaText(node: FlowNode) {
  if (!executionResponse.value?.nodes) return "";
  if (node.kind === "source" && node.sourceFormat === "list") return "Выход: вектор значений";
  if (node.kind === "template") return "Вход: вектор → выход: вектор";
  if (node.kind === "join") return "Вход: вектор значений → текст";
  const input = inputFieldsForNode(executionResponse.value, node.id);
  const output = outputFieldsForNode(executionResponse.value, node.id);
  if (!input.length && !output.length) return "";
  if (node.kind === "source") return `Выход: ${fieldCount(output.length)}`;
  if (node.kind === "output") return `Вход: ${fieldCount(input.length)}`;
  if (!input.length) return `Выход: ${fieldCount(output.length)}`;
  if (!output.length) return `Вход: ${fieldCount(input.length)}`;
  return `Вход: ${input.length} → выход: ${fieldCount(output.length)}`;
}

function requiresValue(operator: FilterOperator) {
  return operator !== "exists" && operator !== "not_exists";
}

function compatibleBlocks(node: FlowNode) {
  const output = graphV2Port(node, "output");
  if (!output) return [];
  return LIBRARY_BLOCKS.filter((block) => {
    if (block.kind === "source") return false;
    const type = block.kind === "fields"
      ? "transform.project"
      : block.kind === "condition"
        ? "transform.filter"
        : block.kind === "template"
          ? "transform.template"
          : block.kind === "join"
            ? "sink.join"
            : `sink.${block.outputFormat ?? "flat"}`;
    const input = BUILTIN_NODE_REGISTRY.get(type)?.ports.find((port) => port.direction === "input");
    return Boolean(input && arePortContractsCompatible(output.contract, input.contract));
  });
}

function continuationBlocks(node: FlowNode) {
  const query = continuationQuery.value.trim().toLocaleLowerCase("ru");
  const blocks = compatibleBlocks(node);
  if (!query) return blocks;
  const stopWords = new Set(["хочу", "нужно", "надо", "добавить", "добавь", "блок", "сделать", "получить", "вывести", "создать", "после", "для", "через"]);
  const tokens = query.split(/[^\p{L}\p{N}]+/u).filter((token) => token.length > 1 && !stopWords.has(token));
  if (!tokens.length) return blocks;
  return blocks.filter((block) => {
    const searchable = `${block.label} ${block.eyebrow} ${block.keywords}`.toLocaleLowerCase("ru");
    return tokens.every((token) => searchable.includes(token) || searchable.includes(token.slice(0, 5)));
  });
}

async function toggleContinuation(nodeId: string) {
  if (continuationFor.value === nodeId) {
    continuationFor.value = "";
    continuationQuery.value = "";
    return;
  }
  continuationFor.value = nodeId;
  continuationQuery.value = "";
  await nextTick();
  board.value
    ?.querySelector<HTMLInputElement>(`[data-node-id="${CSS.escape(nodeId)}"] .continuation-search input`)
    ?.focus();
}

function closeContinuation() {
  continuationFor.value = "";
  continuationQuery.value = "";
}

function setNotice(message: string) {
  notice.value = message;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => {
    notice.value = "Перетащите блок или соедините точки";
  }, 2600);
}

function addNode(
  kind: NodeKind,
  sourceFormat: SourceFormat = "json",
  outputFormat: OutputFormat = "flat",
  originNode?: FlowNode,
) {
  const bounds = board.value?.getBoundingClientRect();
  const centerX = bounds ? (bounds.width / 2 - panX.value) / zoom.value : 600;
  const centerY = bounds ? (bounds.height / 2 - panY.value) / zoom.value : 300;
  const id = `${kind}-${nextNodeId++}`;
  const base: FlowNode = {
    id,
    kind,
    title: `${kind === "source" ? sourceFormat.toUpperCase() : kind === "output" ? LIBRARY_BLOCKS.find((block) => block.outputFormat === outputFormat)?.label : NODE_META[kind].label} ${nextNodeId - 1}`,
    x: originNode
      ? originNode.x + (originNode.kind === "condition" ? 380 : originNode.kind === "source" ? 360 : 340) + 92
      : centerX - 170 + (nextNodeId % 3) * 24,
    y: originNode
      ? originNode.y + outgoingEdges(originNode.id).length * 76
      : centerY - 120 + (nextNodeId % 2) * 28,
  };
  if (kind === "source") {
    base.sourceFormat = sourceFormat;
    base.csvDelimiter = ",";
    base.json = sourceFormat === "csv" ? SAMPLE_CSV : sourceFormat === "list" ? SAMPLE_LIST : SAMPLE_JSON;
  }
  if (kind === "fields") {
    base.selectedPath = "/stores";
    base.selectedFields = ["name"];
  }
  if (kind === "condition") {
    base.filterMode = "all";
    base.conditions = [{ id: nextConditionId++, field: "state", operator: "equal", value: "1" }];
    base.filterExpression = createUiFilterGroup("and", "state");
  }
  if (kind === "template") {
    base.valueTemplate = "0x{value}";
    base.stripOuterQuotes = true;
    base.preview = "";
    base.previewStats = null;
  }
  if (kind === "join") {
    base.delimiter = ",\n";
    base.output = "";
    base.stats = null;
  }
  if (kind === "output") {
    base.delimiter = outputFormat === "template" ? ",\n" : ", ";
    base.csvDelimiter = ",";
    base.csvIncludeHeader = true;
    base.csvQuoteAll = false;
    base.outputFormat = outputFormat;
    base.xmlRoot = "rows";
    base.xmlRow = "row";
    base.tableName = "result";
    base.valueTemplate = outputFormat === "template" ? "0x{value}" : "{value}";
    base.stripOuterQuotes = true;
    base.output = "";
    base.stats = null;
  }
  nodes.value.push(base);
  if (originNode) connectNodes(originNode.id, id);
  selectedNodeId.value = id;
  setNotice(`Блок «${kind === "source" ? sourceFormat.toUpperCase() : kind === "output" ? outputFormat.toUpperCase() : NODE_META[kind].label}» добавлен`);
  scheduleRender();
  return id;
}

function addContinuation(originNode: FlowNode, block: BlockDefinition) {
  addNode(block.kind, block.format, block.outputFormat, originNode);
  closeContinuation();
}

function addFirstContinuation(originNode: FlowNode) {
  const first = continuationBlocks(originNode)[0];
  if (first) addContinuation(originNode, first);
}

function removeNode(id: string) {
  nodes.value = nodes.value.filter((node) => node.id !== id);
  edges.value = edges.value.filter((edge) => edge.from !== id && edge.to !== id);
  delete analyses[id];
  analysisCache.delete(id);
  outputCache.delete(id);
  if (selectedNodeId.value === id) selectedNodeId.value = "";
  if (inspectedNodeId.value === id) closeNodeInspector();
  if (connectingFrom.value === id) connectingFrom.value = "";
  if (continuationFor.value === id) closeContinuation();
  scheduleRender();
}

function removeEdge(id: string) {
  edges.value = edges.value.filter((edge) => edge.id !== id);
  scheduleRender();
}

function handlePort(nodeId: string, direction: PortDirection) {
  if (direction === "output") {
    connectingFrom.value = connectingFrom.value === nodeId ? "" : nodeId;
    setNotice(connectingFrom.value ? "Теперь нажмите вход нужного блока" : "Соединение отменено");
    return;
  }
  if (!connectingFrom.value) {
    setNotice("Сначала нажмите выход блока-источника");
    return;
  }
  if (connectingFrom.value === nodeId) {
    setNotice("Нельзя соединить блок с самим собой");
    return;
  }
  connectNodes(connectingFrom.value, nodeId);
  connectingFrom.value = "";
  scheduleRender();
}

function connectNodes(fromId: string, toId: string) {
  const source = nodeById(fromId);
  const target = nodeById(toId);
  if (!source || !target || !hasOutputPort(source) || !hasInputPort(target)) {
    setNotice("Эти блоки нельзя соединить");
    return;
  }
  const candidateId = `edge-${nextEdgeId}`;
  const preflight = preflightConnection(toGraphV2(), BUILTIN_NODE_REGISTRY, {
    id: candidateId,
    from: { nodeId: fromId, port: graphV2OutputPort(source) },
    to: { nodeId: toId, port: graphV2InputPort(target) },
  });
  if (!preflight.ok) {
    setNotice(preflight.issues[0]?.message ?? "Соединение несовместимо");
    return;
  }
  nextEdgeId += 1;
  edges.value.push({ id: candidateId, from: fromId, to: toId });
  setNotice("Блоки соединены — ветвление поддерживается");
}

function startConnectionDrag(event: PointerEvent, nodeId: string) {
  if (event.button !== 0) return;
  const bounds = board.value?.getBoundingClientRect();
  if (!bounds) return;
  event.preventDefault();
  window.removeEventListener("pointermove", moveConnectionDrag);
  window.removeEventListener("pointerup", finishConnectionDrag);
  connectingFrom.value = nodeId;
  connectionPreview.value = { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
  connectionDrag = { fromId: nodeId, startX: event.clientX, startY: event.clientY, moved: false };
  window.addEventListener("pointermove", moveConnectionDrag);
  window.addEventListener("pointerup", finishConnectionDrag, { once: true });
  setNotice("Тяните линию к входной точке блока");
  scheduleRender();
}

function moveConnectionDrag(event: PointerEvent) {
  if (!connectionDrag) return;
  const bounds = board.value?.getBoundingClientRect();
  if (!bounds) return;
  if (Math.hypot(event.clientX - connectionDrag.startX, event.clientY - connectionDrag.startY) > 4) {
    connectionDrag.moved = true;
  }
  connectionPreview.value = { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
  scheduleRender();
}

function finishConnectionDrag(event: PointerEvent) {
  const drag = connectionDrag;
  window.removeEventListener("pointermove", moveConnectionDrag);
  connectionDrag = null;
  connectionPreview.value = null;
  if (!drag) return;

  const target = document.elementFromPoint(event.clientX, event.clientY) as HTMLElement | null;
  const inputPort = target?.closest<HTMLElement>("[data-input-port]");
  const targetId = inputPort?.dataset.inputPort;
  if (targetId) {
    connectNodes(drag.fromId, targetId);
    connectingFrom.value = "";
  } else if (drag.moved) {
    connectingFrom.value = "";
    setNotice("Связь отменена — отпустите линию на входной точке");
  } else {
    setNotice("Теперь нажмите входную точку нужного блока");
  }
  scheduleRender();
}

function cancelConnection(render = true) {
  window.removeEventListener("pointermove", moveConnectionDrag);
  window.removeEventListener("pointerup", finishConnectionDrag);
  connectionDrag = null;
  connectionPreview.value = null;
  connectingFrom.value = "";
  if (render) scheduleRender();
}

function toggleField(node: FlowNode, field: string) {
  const selected = node.selectedFields ?? [];
  node.selectedFields = selected.includes(field)
    ? selected.filter((name) => name !== field)
    : [...selected, field];
}

function addCondition(node: FlowNode) {
  const fields = availableConditionFields(node.id);
  node.conditions = [
    ...(node.conditions ?? []),
    {
      id: nextConditionId++,
      field: fields.find((field) => field.name === "state")?.name ?? fields[0]?.name ?? "state",
      operator: "equal",
      value: "1",
    },
  ];
}

function removeCondition(node: FlowNode, conditionId: number) {
  node.conditions = (node.conditions ?? []).filter((condition) => condition.id !== conditionId);
}

function startNodeDrag(event: PointerEvent, node: FlowNode) {
  if (event.button !== 0) return;
  closeContinuation();
  selectedNodeId.value = node.id;
  gesture = {
    type: "node",
    nodeId: node.id,
    startX: event.clientX,
    startY: event.clientY,
    originX: node.x,
    originY: node.y,
    moved: false,
  };
  beginGesture();
}

function startPan(event: PointerEvent) {
  if (event.button !== 0 && event.button !== 1) return;
  selectedNodeId.value = "";
  closeNodeInspector();
  closeContinuation();
  connectingFrom.value = "";
  gesture = {
    type: "pan",
    startX: event.clientX,
    startY: event.clientY,
    originX: panX.value,
    originY: panY.value,
  };
  beginGesture();
}

function beginGesture() {
  window.addEventListener("pointermove", moveGesture);
  window.addEventListener("pointerup", endGesture, { once: true });
}

function moveGesture(event: PointerEvent) {
  if (!gesture) return;
  if (gesture.type === "pan") {
    panX.value = gesture.originX + event.clientX - gesture.startX;
    panY.value = gesture.originY + event.clientY - gesture.startY;
  } else {
    const node = nodeById(gesture.nodeId);
    if (!node) return;
    if (Math.hypot(event.clientX - gesture.startX, event.clientY - gesture.startY) > 4) {
      gesture.moved = true;
    }
    node.x = gesture.originX + (event.clientX - gesture.startX) / zoom.value;
    node.y = gesture.originY + (event.clientY - gesture.startY) / zoom.value;
  }
}

function endGesture() {
  if (gesture?.type === "node" && gesture.moved) {
    suppressNodeClickId = gesture.nodeId;
    window.clearTimeout(suppressNodeClickTimer);
    suppressNodeClickTimer = window.setTimeout(() => {
      suppressNodeClickId = "";
    }, 0);
  }
  gesture = null;
  window.removeEventListener("pointermove", moveGesture);
  window.removeEventListener("pointerup", endGesture);
}

function handleWheel(event: WheelEvent) {
  const target = event.target;
  if (target instanceof Element && target.closest(".node-result-output")) return;
  event.preventDefault();
  const bounds = board.value?.getBoundingClientRect();
  if (!bounds) return;
  const pointerX = event.clientX - bounds.left;
  const pointerY = event.clientY - bounds.top;
  const worldX = (pointerX - panX.value) / zoom.value;
  const worldY = (pointerY - panY.value) / zoom.value;
  const nextZoom = Math.min(1.65, Math.max(0.42, zoom.value * Math.exp(-event.deltaY * 0.0012)));
  zoom.value = nextZoom;
  panX.value = pointerX - worldX * nextZoom;
  panY.value = pointerY - worldY * nextZoom;
}

function changeZoom(delta: number) {
  const bounds = board.value?.getBoundingClientRect();
  if (!bounds) return;
  const next = Math.min(1.65, Math.max(0.42, zoom.value + delta));
  const centerX = bounds.width / 2;
  const centerY = bounds.height / 2;
  const worldX = (centerX - panX.value) / zoom.value;
  const worldY = (centerY - panY.value) / zoom.value;
  zoom.value = next;
  panX.value = centerX - worldX * next;
  panY.value = centerY - worldY * next;
}

function fitView() {
  const bounds = board.value?.getBoundingClientRect();
  if (!bounds || !nodes.value.length) return;
  const minX = Math.min(...nodes.value.map((node) => node.x));
  const minY = Math.min(...nodes.value.map((node) => node.y));
  const maxX = Math.max(...nodes.value.map((node) => node.x + (node.kind === "condition" ? 370 : 340)));
  const maxY = Math.max(...nodes.value.map((node) => node.y + 420));
  const availableWidth = Math.max(480, bounds.width - 300);
  const availableHeight = Math.max(360, bounds.height - 110);
  const next = Math.min(1, Math.max(0.42, Math.min(availableWidth / (maxX - minX), availableHeight / (maxY - minY))));
  zoom.value = next;
  panX.value = 270 + (availableWidth - (maxX - minX) * next) / 2 - minX * next;
  panY.value = 70 + (availableHeight - (maxY - minY) * next) / 2 - minY * next;
}

function resetGraph() {
  window.location.reload();
}

function pipelineNodeConfig(node: FlowNode): Record<string, import("./graph-v2").JsonValue> {
  if (node.kind === "source") {
    const downstreamFields = nodes.value.find(
      (candidate) => candidate.kind === "fields"
        && collectAncestors(candidate.id).some((ancestor) => ancestor.id === node.id),
    );
    return {
      title: node.title,
      text: node.json ?? "",
      arrayPath: node.sourceFormat === "list" ? "" : downstreamFields?.selectedPath ?? node.selectedPath ?? "",
      ...(node.sourceFormat === "csv"
        ? { delimiter: node.csvDelimiter ?? ",", includeHeader: true }
        : {}),
    };
  }
  if (node.kind === "fields") {
    return { title: node.title, fields: node.selectedFields ?? [] };
  }
  if (node.kind === "condition") {
    const expression = node.filterExpression ? normalizeFilterExpression(node.filterExpression) : null;
    return {
      title: node.title,
      ...(expression ? { expression } : {}),
    } as unknown as Record<string, import("./graph-v2").JsonValue>;
  }
  if (node.kind === "template") {
    return {
      title: node.title,
      template: node.valueTemplate ?? "0x{value}",
      stripOuterQuotes: node.stripOuterQuotes !== false,
      skipEmpty: true,
    };
  }
  if (node.kind === "join") {
    return {
      title: node.title,
      delimiter: node.delimiter ?? ",\n",
    };
  }
  return {
    title: node.title,
    format: node.outputFormat ?? "flat",
    delimiter: node.delimiter ?? ", ",
    skipEmpty: true,
    unique: false,
    csvDelimiter: node.csvDelimiter ?? ",",
    csvIncludeHeader: node.csvIncludeHeader !== false,
    csvQuoteAll: node.csvQuoteAll === true,
    xmlRoot: node.xmlRoot ?? "rows",
    xmlRow: node.xmlRow ?? "row",
    tableName: node.tableName ?? "result",
    valueTemplate: node.valueTemplate ?? "{value}",
    stripOuterQuotes: node.stripOuterQuotes !== false,
  };
}

function normalizePipelineTitle(value: unknown): string {
  if (typeof value !== "string") return "Обработка магазинов";
  const trimmed = value.trim();
  return trimmed || "Обработка магазинов";
}

function sanitizePersistedNode(node: FlowNode): FlowNode {
  const {
    output: _output,
    stats: _stats,
    preview: _preview,
    previewStats: _previewStats,
    previewError: _previewError,
    error: _error,
    ...rest
  } = node;
  return rest as FlowNode;
}

function clearPersistedPipeline() {
  if (typeof window === "undefined") return;
  try {
    localStorage.removeItem(PIPELINE_STORAGE_KEY);
  } catch {
    // Ignore storage errors so auto-save failures stay best-effort.
  }
}

function readPersistedPipelineRaw(): string | null {
  if (typeof window === "undefined") return null;
  try {
    return localStorage.getItem(PIPELINE_STORAGE_KEY);
  } catch {
    return null;
  }
}

function createPipelineV2(): PipelineFileV2 {
  const graph = toGraphV2();
  return {
    format: PIPELINE_FORMAT,
    version: 2,
    savedAt: new Date().toISOString(),
    metadata: { name: normalizePipelineTitle(pipelineTitle.value) },
    view: {
      x: panX.value,
      y: panY.value,
      zoom: zoom.value,
      ...(selectedNodeId.value ? { selectedNodeIds: [selectedNodeId.value] } : {}),
    },
    graph: {
      ...graph,
      nodes: graph.nodes.map((graphNode) => {
        const node = nodeById(graphNode.id);
        return { ...graphNode, config: node ? pipelineNodeConfig(node) : {} };
      }),
    },
  };
}

function persistCurrentPipeline() {
  if (typeof window === "undefined") return;
  const snapshot = {
    version: 1,
    title: normalizePipelineTitle(pipelineTitle.value),
    view: { panX: panX.value, panY: panY.value, zoom: zoom.value },
    nodes: nodes.value.map(sanitizePersistedNode),
    edges: edges.value,
  };
  try {
    localStorage.setItem(PIPELINE_STORAGE_KEY, JSON.stringify(snapshot));
  } catch {
    clearPersistedPipeline();
  }
}

function isValidStoredPipeline(value: unknown): value is LoadedPipeline {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const candidate = value as Record<string, unknown>;
  if (candidate.version !== 1) return false;
  if (typeof candidate.title !== "undefined" && typeof candidate.title !== "string") return false;
  if (!candidate.view || typeof candidate.view !== "object" || Array.isArray(candidate.view)) return false;
  const view = candidate.view as Record<string, unknown>;
  if (typeof view.panX !== "number" || typeof view.panY !== "number" || typeof view.zoom !== "number"
    || !Number.isFinite(view.panX) || !Number.isFinite(view.panY) || !Number.isFinite(view.zoom)) return false;
  if (!Array.isArray(candidate.nodes) || !Array.isArray(candidate.edges)) return false;
  const validNodeKinds = new Set(["source", "fields", "condition", "template", "join", "output"]);
  return candidate.nodes.every((node) => {
    if (!node || typeof node !== "object" || Array.isArray(node)) return false;
    const candidateNode = node as Record<string, unknown>;
    if (candidateNode.selectedPath !== undefined && typeof candidateNode.selectedPath !== "string") return false;
    if (candidateNode.selectedFields !== undefined && (!Array.isArray(candidateNode.selectedFields) || !candidateNode.selectedFields.every((field) => typeof field === "string"))) return false;
    if (candidateNode.conditions !== undefined && !Array.isArray(candidateNode.conditions)) return false;
    if (candidateNode.filterExpression !== undefined && (!candidateNode.filterExpression || typeof candidateNode.filterExpression !== "object" || Array.isArray(candidateNode.filterExpression))) return false;
    return typeof candidateNode.id === "string"
      && typeof candidateNode.kind === "string"
      && validNodeKinds.has(candidateNode.kind as string)
      && typeof candidateNode.title === "string"
      && typeof candidateNode.x === "number"
      && typeof candidateNode.y === "number"
      && Number.isFinite(candidateNode.x)
      && Number.isFinite(candidateNode.y);
  }) && candidate.edges.every((edge) => {
    if (!edge || typeof edge !== "object" || Array.isArray(edge)) return false;
    const candidateEdge = edge as Record<string, unknown>;
    return typeof candidateEdge.id === "string"
      && typeof candidateEdge.from === "string"
      && typeof candidateEdge.to === "string";
  });
}

function readPersistedPipeline(): LoadedPipeline | null {
  const raw = readPersistedPipelineRaw();
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw);
    if (!isValidStoredPipeline(parsed)) {
      clearPersistedPipeline();
      return null;
    }
    return {
      title: normalizePipelineTitle(parsed.title),
      view: {
        panX: parsed.view.panX,
        panY: parsed.view.panY,
        zoom: parsed.view.zoom,
      },
      nodes: parsed.nodes.map((node: FlowNode) => node.kind === "output" && node.outputFormat === "template"
        ? {
            ...node,
            kind: "template" as const,
            outputFormat: undefined,
            delimiter: undefined,
          }
        : node),
      edges: parsed.edges,
    };
  } catch {
    clearPersistedPipeline();
    return null;
  }
}

function applyLoadedPipeline(pipeline: LoadedPipeline, options: { preserveSelection?: boolean; run?: boolean; noticeMessage?: string } = {}) {
  const { preserveSelection = false, run = true, noticeMessage } = options;
  executionToken += 1;
  nodes.value = pipeline.nodes;
  edges.value = pipeline.edges;
  panX.value = pipeline.view.panX;
  panY.value = pipeline.view.panY;
  zoom.value = pipeline.view.zoom;
  pipelineTitle.value = normalizePipelineTitle(pipeline.title);
  if (!preserveSelection) selectedNodeId.value = "";
  closeNodeInspector();
  closeContinuation();
  cancelConnection(false);
  for (const key of Object.keys(analyses)) delete analyses[key];
  clearExecutionResults();
  analysisCache.clear();
  outputCache.clear();
  previewCache.clear();
  nextNodeId = Math.max(1, ...pipeline.nodes.map((node) => Number(node.id.match(/(\d+)$/)?.[1] ?? 0))) + 1;
  nextEdgeId = Math.max(1, ...pipeline.edges.map((edge) => Number(edge.id.match(/(\d+)$/)?.[1] ?? 0))) + 1;
  nextConditionId = Math.max(1, ...pipeline.nodes.flatMap((node) => Array.isArray(node.conditions) ? node.conditions.map((condition) => condition.id) : [])) + 1;
  scheduleRender();
  if (run) scheduleExecute(true);
  if (noticeMessage) setNotice(noticeMessage);
}

function applyPreset(presetId: string) {
  const preset = PIPELINE_PRESETS.find((candidate) => candidate.id === presetId);
  if (!preset) return;
  const pipeline = preset.build();
  applyLoadedPipeline(pipeline, { noticeMessage: `Пресет «${preset.title}» загружен` });
  persistCurrentPipeline();
}

let persistTimer: number | undefined;
function schedulePersist() {
  window.clearTimeout(persistTimer);
  persistTimer = window.setTimeout(() => {
    persistCurrentPipeline();
  }, 300);
}

function savePipelineFile() {
  const blob = new Blob([encodePipelineV2(createPipelineV2(), { pretty: true })], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  const stamp = new Date().toISOString().slice(0, 19).replaceAll(":", "-");
  link.href = url;
  link.download = `peregon-${stamp}.json`;
  link.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
  setNotice("Схема сохранена в файл");
}

function openPipelineFile() {
  if (!pipelineFileInput.value) return;
  pipelineFileInput.value.value = "";
  pipelineFileInput.value.click();
}

function sourcePathForGraphNode(pipeline: PipelineFileV2, nodeId: string) {
  const visited = new Set<string>();
  let current = nodeId;
  while (!visited.has(current)) {
    visited.add(current);
    const incoming = pipeline.graph.connections.find((connection) => connection.to.nodeId === current);
    if (!incoming) return "";
    const parent = pipeline.graph.nodes.find((node) => node.id === incoming.from.nodeId);
    if (!parent) return "";
    if (parent.type.startsWith("source.")) {
      return typeof parent.config.arrayPath === "string" ? parent.config.arrayPath : "";
    }
    current = parent.id;
  }
  return "";
}

function parsePipelineFile(value: unknown): LoadedPipeline {
  const rawVersion = value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>).version
    : undefined;
  const pipeline = migratePipelineFile(rawVersion === 2 ? decodePipelineV2(value) : value);
  const title = typeof (pipeline as { metadata?: { name?: unknown } }).metadata?.name === "string"
    ? (pipeline as { metadata?: { name?: string } }).metadata?.name
    : undefined;
  const importedNodes: FlowNode[] = pipeline.graph.nodes.map((graphNode): FlowNode => {
    const config = graphNode.config;
    const kind: NodeKind = graphNode.type.startsWith("source.")
      ? "source"
      : graphNode.type === "transform.project"
        ? "fields"
        : graphNode.type === "transform.filter"
          ? "condition"
          : graphNode.type === "transform.template" || graphNode.type === "sink.template"
            ? "template"
            : graphNode.type === "sink.join"
              ? "join"
          : "output";
    const imported: FlowNode = {
      id: graphNode.id,
      kind,
      title: typeof config.title === "string" ? config.title : graphNode.type,
      x: graphNode.position.x,
      y: graphNode.position.y,
    };
    if (kind === "source") {
      imported.sourceFormat = graphNode.type === "source.csv"
        ? "csv"
        : graphNode.type === "source.list" ? "list" : "json";
      imported.json = typeof config.text === "string" ? config.text : "";
      imported.csvDelimiter = typeof config.delimiter === "string" ? config.delimiter : ",";
      imported.selectedPath = typeof config.arrayPath === "string" ? config.arrayPath : "";
    } else if (kind === "fields") {
      imported.selectedFields = Array.isArray(config.fields)
        ? config.fields.filter((field): field is string => typeof field === "string")
        : [];
      imported.selectedPath = sourcePathForGraphNode(pipeline, graphNode.id);
    } else if (kind === "condition") {
      const expression = normalizeFilterExpression(config.expression);
      imported.filterExpression = expression ? filterExpressionToUi(expression) : undefined;
      imported.filterMode = "all";
      imported.conditions = [];
    } else if (kind === "template") {
      imported.valueTemplate = typeof config.template === "string"
        ? config.template
        : typeof config.valueTemplate === "string" ? config.valueTemplate : "0x{value}";
      imported.stripOuterQuotes = config.stripOuterQuotes !== false;
      imported.preview = "";
      imported.previewStats = null;
    } else if (kind === "join") {
      imported.delimiter = typeof config.delimiter === "string" ? config.delimiter : ",\n";
      imported.output = "";
      imported.stats = null;
    } else {
      const format = graphNode.type.slice("sink.".length);
      imported.outputFormat = ["flat", "template", "json", "csv", "xml", "sql"].includes(format)
        ? format as OutputFormat
        : "flat";
      imported.delimiter = typeof config.delimiter === "string" ? config.delimiter : ", ";
      imported.csvDelimiter = typeof config.csvDelimiter === "string" ? config.csvDelimiter : ",";
      imported.csvIncludeHeader = config.csvIncludeHeader !== false;
      imported.csvQuoteAll = config.csvQuoteAll === true;
      imported.xmlRoot = typeof config.xmlRoot === "string" ? config.xmlRoot : "rows";
      imported.xmlRow = typeof config.xmlRow === "string" ? config.xmlRow : "row";
      imported.tableName = typeof config.tableName === "string" ? config.tableName : "result";
      imported.valueTemplate = typeof config.valueTemplate === "string"
        ? config.valueTemplate
        : imported.outputFormat === "template" ? "0x{value}" : "{value}";
      imported.stripOuterQuotes = config.stripOuterQuotes !== false;
      imported.output = "";
      imported.stats = null;
    }
    return imported;
  });
  return {
    title,
    view: { panX: pipeline.view.x, panY: pipeline.view.y, zoom: pipeline.view.zoom },
    nodes: importedNodes,
    edges: pipeline.graph.connections.map((connection) => ({
      id: connection.id,
      from: connection.from.nodeId,
      to: connection.to.nodeId,
    })),
  };
}

async function loadPipelineFile(event: Event) {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  if (!file) return;
  try {
    if (file.size > 10 * 1024 * 1024) throw new Error("Файл больше 10 МБ");
    const pipeline = parsePipelineFile(JSON.parse(await file.text()));
    applyLoadedPipeline(pipeline, { noticeMessage: `Загружено: ${pipeline.nodes.length} блоков, ${pipeline.edges.length} связей` });
    await nextTick();
    persistCurrentPipeline();
  } catch (error) {
    setNotice(error instanceof Error ? error.message : "Не удалось загрузить файл");
  } finally {
    input.value = "";
  }
}

function scheduleRender() {
  cancelAnimationFrame(renderFrame);
  renderFrame = requestAnimationFrame(async () => {
    await nextTick();
    renderSurface();
  });
}

function edgeGeometry(fromX: number, fromY: number, toX: number, toY: number) {
  const distance = Math.max(80, Math.abs(toX - fromX) * 0.52);
  const control1X = fromX + distance;
  const control2X = toX - distance;
  return {
    path: `M ${fromX} ${fromY} C ${control1X} ${fromY}, ${control2X} ${toY}, ${toX} ${toY}`,
    midX: (fromX + 3 * control1X + 3 * control2X + toX) / 8,
    midY: (fromY + toY) / 2,
  };
}

function renderSurface() {
  const canvas = surface.value;
  const boardElement = board.value;
  if (!canvas || !boardElement || !renderer) return;
  const canvasRect = canvas.getBoundingClientRect();
  const surfaceEdges: SurfaceEdge[] = [];
  const visuals: EdgeVisual[] = [];
  const colors: Record<NodeKind, [number, number, number, number]> = {
    source: [0.4, 0.34, 0.85, 0.92],
    fields: [0.21, 0.5, 0.73, 0.92],
    condition: [0.08, 0.57, 0.53, 0.92],
    template: [0.82, 0.42, 0.21, 0.92],
    join: [0.82, 0.42, 0.21, 0.92],
    output: [0.82, 0.42, 0.21, 0.92],
  };
  for (const edge of edges.value) {
    const fromPort = boardElement.querySelector<HTMLElement>(`[data-output-port="${edge.from}"]`);
    const toPort = boardElement.querySelector<HTMLElement>(`[data-input-port="${edge.to}"]`);
    if (!fromPort || !toPort) continue;
    const fromRect = fromPort.getBoundingClientRect();
    const toRect = toPort.getBoundingClientRect();
    const kind = nodeById(edge.from)?.kind ?? "source";
    const positioned = {
      fromX: fromRect.left + fromRect.width / 2 - canvasRect.left,
      fromY: fromRect.top + fromRect.height / 2 - canvasRect.top,
      toX: toRect.left + toRect.width / 2 - canvasRect.left,
      toY: toRect.top + toRect.height / 2 - canvasRect.top,
      color: colors[kind],
    };
    surfaceEdges.push(positioned);
    visuals.push({ ...edge, ...edgeGeometry(positioned.fromX, positioned.fromY, positioned.toX, positioned.toY) });
  }

  if (connectingFrom.value && connectionPreview.value) {
    const fromPort = boardElement.querySelector<HTMLElement>(`[data-output-port="${connectingFrom.value}"]`);
    if (fromPort) {
      const fromRect = fromPort.getBoundingClientRect();
      const kind = nodeById(connectingFrom.value)?.kind ?? "source";
      surfaceEdges.push({
        fromX: fromRect.left + fromRect.width / 2 - canvasRect.left,
        fromY: fromRect.top + fromRect.height / 2 - canvasRect.top,
        toX: connectionPreview.value.x,
        toY: connectionPreview.value.y,
        color: colors[kind],
      });
    }
  }
  edgeVisuals.value = visuals;
  renderer.render({ panX: panX.value, panY: panY.value, zoom: zoom.value, edges: surfaceEdges, theme: theme.value });
}

function scheduleExecute(immediate = false) {
  window.clearTimeout(executeTimer);
  executeTimer = window.setTimeout(executeGraph, immediate ? 0 : 480);
}

function applyAnalysisResult(sourceNode: FlowNode, response: AnalyzeResponse) {
  sourceNode.error = "";
  if (!response.ok) {
    delete analyses[sourceNode.id];
    sourceNode.error = response.error.message;
    return;
  }
  analyses[sourceNode.id] = response;
}

function applyOutputResult(outputNode: FlowNode, response: TransformResponse) {
  outputNode.error = "";
  if (!response.ok) {
    outputNode.output = "";
    outputNode.stats = null;
    outputNode.error = response.error.message;
    return;
  }
  outputNode.output = response.output;
  outputNode.stats = response;
}

function applyPreviewResult(node: FlowNode, response: TransformResponse) {
  node.previewError = "";
  if (!response.ok) {
    node.preview = "";
    node.previewStats = null;
    node.previewError = response.error.message;
    return;
  }
  node.preview = response.output;
  node.previewStats = response;
}

function planStats(result: ExecutePlanNodeResult): TransformSuccess {
  return {
    ok: true,
    output: "",
    source_items: result.stats.input_items,
    object_items: result.stats.input_items,
    matched_items: result.stats.output_items,
    filtered_out: result.stats.filtered_out,
    skipped_items: result.stats.skipped_items,
    empty_values: result.stats.empty_values ?? 0,
    values: result.stats.values ?? result.stats.output_items,
  };
}

function applyPlanNodeResult(node: FlowNode, result: ExecutePlanNodeResult | undefined, sinkOutput?: string) {
  const message = result?.diagnostics[0]?.message ?? "Блок не был выполнен";
  if (!result?.ok) {
    if (node.kind === "output" || node.kind === "join") {
      node.output = "";
      node.stats = null;
      node.error = message;
    } else if (node.kind === "fields" || node.kind === "condition" || node.kind === "source" || node.kind === "template") {
      node.preview = "";
      node.previewStats = null;
      node.previewError = message;
    } else {
      node.error = message;
    }
    return;
  }

  if (node.kind === "output" || node.kind === "join") {
    node.output = sinkOutput ?? "";
    node.stats = planStats(result);
    node.error = "";
  } else if (node.kind === "fields" || node.kind === "condition" || node.kind === "source" || node.kind === "template") {
    node.preview = node.kind === "template" || node.kind === "source" && node.sourceFormat === "list"
      ? listPreviewContent(result)
      : JSON.stringify(result.preview, null, 2);
    node.previewStats = planStats(result);
    node.previewError = "";
  } else {
    node.error = "";
  }
}

async function executeGraph() {
  if (!client) return;
  const token = ++executionToken;
  const startedAt = performance.now();
  clearExecutionResults();
  isRunning.value = true;
  cachedBranches.value = 0;

  try {
    const sourceNodes = nodes.value.filter((node) => node.kind === "source");
    await Promise.all(
      sourceNodes.map(async (sourceNode) => {
        const signature = JSON.stringify({
          data: sourceNode.json ?? "",
          format: sourceNode.sourceFormat ?? "json",
          csvDelimiter: sourceNode.csvDelimiter ?? ",",
        });
        const cached = analysisCache.get(sourceNode.id);
        if (cached?.signature === signature) {
          engineVersion.value = cached.engineVersion;
          applyAnalysisResult(sourceNode, cached.response);
          return;
        }
        const reply = await client!.request<AnalyzeResponse>({
          action: "analyze",
          json: sourceNode.json ?? "",
          source_format: sourceNode.sourceFormat ?? "json",
          csv_delimiter: sourceNode.csvDelimiter ?? ",",
        });
        if (token !== executionToken) return;
        analysisCache.set(sourceNode.id, {
          signature,
          response: reply.response,
          engineVersion: reply.engineVersion,
        });
        engineVersion.value = reply.engineVersion;
        applyAnalysisResult(sourceNode, reply.response);
      }),
    );
    if (token !== executionToken) return;

    for (const fieldNode of nodes.value.filter((node) => node.kind === "fields")) {
      const availableArrays = arraysForNode(fieldNode.id);
      const selectedArray =
        availableArrays.find((candidate) => candidate.path === fieldNode.selectedPath) ?? availableArrays[0];
      if (selectedArray && fieldNode.selectedPath !== selectedArray.path) fieldNode.selectedPath = selectedArray.path;
      const availableNames = new Set(selectedArray?.fields.map((field) => field.name) ?? []);
      fieldNode.selectedFields = (fieldNode.selectedFields ?? []).filter((field) => availableNames.has(field));
      if (!fieldNode.selectedFields.length && selectedArray?.fields.length) {
        fieldNode.selectedFields = [
          selectedArray.fields.find((field) => field.name === "name")?.name ?? selectedArray.fields[0].name,
        ];
      }
    }

    const connectedIds = new Set(edges.value.flatMap((edge) => [edge.from, edge.to]));
    const activeNodes = nodes.value.filter((node) => connectedIds.has(node.id));
    const activeEdges = edges.value.filter(
      (edge) => connectedIds.has(edge.from) && connectedIds.has(edge.to),
    );
    for (const node of nodes.value) {
      if (connectedIds.has(node.id) || node.kind === "source") continue;
      if (node.kind === "output" || node.kind === "join") {
        node.output = "";
        node.stats = null;
        node.error = "Соедините источник данных";
      } else if (node.kind === "fields" || node.kind === "condition" || node.kind === "template") {
        node.preview = "";
        node.previewStats = null;
        node.previewError = "Подключите источник данных";
      }
    }
    if (!activeNodes.length) return;

    const legacyNodes = activeNodes.map((node) => {
      if (node.kind !== "source") return node;
      const downstreamFields = nodes.value.find(
        (candidate) => candidate.kind === "fields"
          && collectAncestors(candidate.id).some((ancestor) => ancestor.id === node.id),
      );
      const fallbackPath = analyses[node.id]?.array_paths[0]?.path ?? "";
      return { ...node, selectedPath: downstreamFields?.selectedPath ?? fallbackPath };
    });
    const request = buildExecutionPlanRequest(toGraphV2(activeNodes, activeEdges), {
      legacyNodes,
      previewLimit: 50,
    });
    const reply = await client.request<ExecutePlanResponse>(request);
    if (token !== executionToken) return;
    engineVersion.value = reply.engineVersion;
    executionResponse.value = reply.response;
    cachedBranches.value = activeNodes.reduce(
      (count, node) => count + (reply.response.nodes[node.id]?.cached ? 1 : 0),
      0,
    );
    for (const node of activeNodes) {
      const result = reply.response.nodes[node.id];
      executionResults[node.id] = result;
      applyPlanNodeResult(node, result, reply.response.sink_outputs[node.id]);
    }
  } catch (error) {
    setNotice(error instanceof Error ? error.message : "Не удалось выполнить граф");
  } finally {
    if (token === executionToken) {
      isRunning.value = false;
      lastRunMs.value = performance.now() - startedAt;
      scheduleRender();
    }
  }
}

function handleKeydown(event: KeyboardEvent) {
  const target = event.target as HTMLElement | null;
  const editing = target?.matches("input, textarea, select, [contenteditable='true']");
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
    event.preventDefault();
    savePipelineFile();
    return;
  }
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "o") {
    event.preventDefault();
    openPipelineFile();
    return;
  }
  if (event.key === "Escape" && continuationFor.value) {
    event.preventDefault();
    closeContinuation();
    return;
  }
  if (event.key === "Escape" && inspectedNodeId.value) {
    event.preventDefault();
    closeNodeInspector();
    return;
  }
  if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
    event.preventDefault();
    executeGraph();
    return;
  }
  if (!editing && (event.key === "Delete" || event.key === "Backspace") && selectedNodeId.value) {
    event.preventDefault();
    removeNode(selectedNodeId.value);
  }
  if (!editing && event.key === "Escape") cancelConnection();
}

watch(geometrySignature, scheduleRender);
watch(executionSignature, () => {
  executionToken += 1;
  clearExecutionResults();
  scheduleExecute();
});
watch(persistSignature, () => {
  schedulePersist();
});

onMounted(async () => {
  const savedTheme = localStorage.getItem("peregon-theme");
  applyTheme(savedTheme === "light" || savedTheme === "dark"
    ? savedTheme
    : window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  client = new JsonEngineClient();
  if (surface.value) {
    renderer = new FlowSurfaceRenderer(surface.value);
    gpuMode.value = await renderer.init();
  }
  resizeObserver = new ResizeObserver(scheduleRender);
  if (board.value) resizeObserver.observe(board.value);
  window.addEventListener("keydown", handleKeydown);
  await nextTick();
  const persisted = readPersistedPipeline();
  if (persisted) {
    applyLoadedPipeline(persisted, { preserveSelection: true, run: false, noticeMessage: "Восстановлено локальное состояние" });
  } else {
    fitView();
  }
  persistCurrentPipeline();
  scheduleExecute(true);
  scheduleRender();
});

onBeforeUnmount(() => {
  executionToken += 1;
  window.clearTimeout(executeTimer);
  window.clearTimeout(noticeTimer);
  window.clearTimeout(persistTimer);
  cancelAnimationFrame(renderFrame);
  endGesture();
  window.clearTimeout(suppressNodeClickTimer);
  cancelConnection(false);
  resizeObserver?.disconnect();
  renderer?.destroy();
  client?.terminate();
  window.removeEventListener("keydown", handleKeydown);
});
</script>

<template>
  <div class="flow-app">
    <header class="flow-topbar">
      <div class="flow-brand">
        <span class="flow-brand-mark">P</span>
        <span class="flow-brand-copy">
          <strong>PEREGON</strong>
          <small>Visual pipeline</small>
        </span>
        <details class="version-menu">
          <summary :aria-label="`Версии Peregon ${VERSION_INFO.project.version} и пакетов`">
            v{{ VERSION_INFO.project.version }}
          </summary>
          <div class="version-popover">
            <div class="version-popover-heading">
              <strong>Версии</strong>
              <small>Текущая сборка</small>
            </div>
            <section v-for="group in VERSION_GROUPS" :key="group.label">
              <h2>{{ group.label }}</h2>
              <dl>
                <template v-for="packageInfo in group.packages" :key="packageInfo.name">
                  <dt>{{ packageInfo.name }}</dt>
                  <dd>{{ packageInfo.version }}</dd>
                </template>
              </dl>
            </section>
          </div>
        </details>
      </div>

      <div class="flow-document-title">
        <span class="save-dot"></span>
        <strong>{{ pipelineTitle }}</strong>
        <small>{{ nodes.length }} блоков · {{ edges.length }} связей</small>
      </div>

      <div class="flow-top-actions">
        <button
          type="button"
          class="theme-toggle"
          :aria-label="theme === 'dark' ? 'Включить светлую тему' : 'Включить тёмную тему'"
          :title="theme === 'dark' ? 'Светлая тема' : 'Тёмная тема'"
          @click="toggleTheme"
        >
          <span aria-hidden="true">{{ theme === "dark" ? "☀" : "☾" }}</span>
        </button>
        <div class="file-actions" role="group" aria-label="Файл схемы">
          <button type="button" title="Открыть схему (Ctrl/⌘ O)" @click="openPipelineFile">
            <span>↑</span><span class="file-label">Открыть</span>
          </button>
          <button type="button" title="Сохранить схему (Ctrl/⌘ S)" @click="savePipelineFile">
            <span>↓</span><span class="file-label">Сохранить</span>
          </button>
          <input
            ref="pipelineFileInput"
            class="file-input"
            type="file"
            accept="application/json,.json"
            tabindex="-1"
            aria-hidden="true"
            @change="loadPipelineFile"
          />
        </div>
        <span class="runtime-pill" :class="gpuMode">
          <i></i>{{ gpuMode === "webgpu" ? "WebGPU" : gpuMode === "canvas" ? "Canvas fallback" : "GPU…" }}
        </span>
        <span class="runtime-pill wasm" :class="{ ready: engineVersion }">
          <i></i>{{ engineVersion ? `WASM ${engineVersion}` : "WASM…" }}
        </span>
        <button type="button" class="run-button" :disabled="isRunning" @click="executeGraph">
          <span>{{ isRunning ? "•••" : "▶" }}</span>
          {{ isRunning ? "Выполняется" : "Запустить" }}
        </button>
      </div>
    </header>

    <main
      ref="board"
      class="flow-board"
      :class="{ connecting: connectingFrom, dragging: gesture }"
      @pointerdown="startPan"
      @wheel="handleWheel"
    >
      <canvas ref="surface" class="flow-surface" aria-hidden="true"></canvas>
      <svg class="edge-interactions" aria-label="Связи между блоками">
        <g v-for="edge in edgeVisuals" :key="edge.id" class="edge-hit">
          <path class="edge-hit-path" :d="edge.path" @pointerdown.stop />
          <g
            class="edge-delete"
            :transform="`translate(${edge.midX} ${edge.midY})`"
            role="button"
            tabindex="0"
            :aria-label="`Удалить связь ${nodeById(edge.from)?.title ?? ''} — ${nodeById(edge.to)?.title ?? ''}`"
            @pointerdown.stop
            @click.stop="removeEdge(edge.id)"
            @keydown.enter.prevent="removeEdge(edge.id)"
            @keydown.space.prevent="removeEdge(edge.id)"
          >
            <circle r="11"></circle>
            <path d="M -3.5 -3.5 L 3.5 3.5 M 3.5 -3.5 L -3.5 3.5"></path>
          </g>
        </g>
      </svg>

      <aside class="block-library" aria-label="Библиотека блоков" @pointerdown.stop>
        <div class="library-heading">
          <span>Блоки</span>
          <strong>Добавить на холст</strong>
        </div>
        <div class="library-presets" role="group" aria-label="Пресеты схемы">
          <span>Пресеты</span>
          <div class="preset-list">
            <button
              v-for="preset in PIPELINE_PRESETS"
              :key="preset.id"
              type="button"
              class="preset-pill"
              @click="applyPreset(preset.id)"
            >
              <strong>{{ preset.title }}</strong>
              <small>{{ preset.description }}</small>
            </button>
          </div>
        </div>
        <div class="library-list">
          <button
            v-for="block in LIBRARY_BLOCKS"
            :key="block.key"
            type="button"
            class="library-item"
            :style="{ '--node-color': block.color }"
            @click="addNode(block.kind, block.format, block.outputFormat)"
          >
            <span class="library-icon">{{ block.icon }}</span>
            <span>
              <strong>{{ block.label }}</strong>
              <small>{{ block.eyebrow }}</small>
            </span>
            <b>+</b>
          </button>
        </div>
        <div class="library-tip">
          <span>⌘</span>
          <p>Тяните связь от точки к точке. Наведите на линию, чтобы удалить.</p>
        </div>
      </aside>

      <div
        class="node-layer"
        :style="{ transform: `translate(${panX}px, ${panY}px) scale(${zoom})` }"
      >
        <article
          v-for="node in nodes"
          :key="node.id"
          :data-node-id="node.id"
          class="flow-node"
          :class="[`node-${node.kind}`, { selected: selectedNodeId === node.id, 'continuation-open': continuationFor === node.id }]"
          :style="{
            transform: `translate(${node.x}px, ${node.y}px)`,
            '--node-color': nodeDisplayMeta(node).color,
          }"
          @pointerdown.stop="selectedNodeId = node.id"
          @click.stop="handleNodeCardClick($event, node)"
        >
          <button
            v-if="hasInputPort(node)"
            type="button"
            class="node-port input-port"
            :class="{ awaiting: connectingFrom }"
            :data-input-port="node.id"
            :aria-label="`Подключить вход блока ${node.title}`"
            @pointerdown.stop
            @click.stop="handlePort(node.id, 'input')"
          ></button>

          <header class="node-heading" @pointerdown.stop="startNodeDrag($event, node)">
            <span class="node-icon">{{ nodeDisplayMeta(node).icon }}</span>
            <div>
              <small>{{ nodeDisplayMeta(node).eyebrow }}</small>
              <input v-model="node.title" aria-label="Название блока" @pointerdown.stop />
            </div>
            <button
              type="button"
              class="node-inspect"
              :aria-label="`Показать вход и выход блока ${node.title}`"
              title="Вход / выход"
              @pointerdown.stop
              @click.stop="openNodeInspector(node)"
            >↗</button>
            <button
              type="button"
              class="node-delete"
              :aria-label="`Удалить блок ${node.title}`"
              @pointerdown.stop
              @click.stop="removeNode(node.id)"
            >×</button>
          </header>

          <div class="node-body" @pointerdown.stop>
            <div v-if="nodeSchemaText(node)" class="node-schema-summary">
              <span>Схема</span>
              <code>{{ nodeSchemaText(node) }}</code>
              <small v-if="executionResults[node.id]?.cached" class="node-cache-badge">кэш</small>
            </div>
            <template v-if="node.kind === 'source'">
              <div class="source-format-row">
                <span>Формат</span>
                <strong>{{ (node.sourceFormat ?? 'json').toUpperCase() }}</strong>
                <label v-if="node.sourceFormat === 'csv'">
                  Разделитель
                  <input v-model="node.csvDelimiter" maxlength="1" aria-label="Разделитель CSV" />
                </label>
              </div>
              <div class="node-label-row">
                <span>{{ (node.sourceFormat ?? 'json').toUpperCase() }}</span>
                <small v-if="analyses[node.id]" class="node-ok">корректный</small>
                <small v-else-if="node.error" class="node-bad">ошибка</small>
              </div>
              <JsonCodeEditor
                v-if="(node.sourceFormat ?? 'json') === 'json'"
                v-model="node.json"
                :label="`Исходные данные ${node.sourceFormat ?? 'json'}`"
              />
              <textarea
                v-else
                v-model="node.json"
                class="node-code-input"
                spellcheck="false"
                :aria-label="`Исходные данные ${node.sourceFormat ?? 'json'}`"
              ></textarea>
              <p v-if="node.error" class="node-error">{{ node.error }}</p>
              <div v-else-if="analyses[node.id]" class="source-stats">
                <span>{{ analyses[node.id]?.array_paths.length }} набор</span>
                <span>{{ (node.json ?? '').length }} символов</span>
              </div>
              <div class="node-label-row step-result-label">
                <span>Результат</span>
                <small v-if="node.previewStats" class="node-ok">{{ node.previewStats.matched_items }} строк</small>
              </div>
              <PlainTextViewer
                v-if="node.sourceFormat === 'list'"
                class="node-step-preview"
                :value="node.preview || (node.previewError ? '' : 'Результат появится здесь')"
                :label="`Результат блока ${node.title}`"
              />
              <JsonCodeEditor
                v-else
                class="node-result-output node-step-preview"
                :model-value="node.preview || (node.previewError ? '' : 'Результат появится здесь')"
                :highlight-syntax="Boolean(node.preview)"
                readonly
                :label="`Результат блока ${node.title}`"
              />
              <p v-if="node.previewError" class="node-error">{{ node.previewError }}</p>
            </template>

            <template v-else-if="node.kind === 'fields'">
              <label class="node-control">
                <span>Массив</span>
                <select v-model="node.selectedPath">
                  <option
                    v-for="array in arraysForNode(node.id)"
                    :key="array.path"
                    :value="array.path"
                  >{{ array.label }} · {{ array.items }}</option>
                </select>
              </label>
              <div class="node-label-row fields-label">
                <span>Оставить поля</span>
                <small>{{ node.selectedFields?.length ?? 0 }} выбрано</small>
              </div>
              <div v-if="fieldsForNode(node).length" class="node-field-list">
                <button
                  v-for="field in fieldsForNode(node)"
                  :key="field.name"
                  type="button"
                  :class="{ active: node.selectedFields?.includes(field.name) }"
                  @click="toggleField(node, field.name)"
                >
                  <span>{{ node.selectedFields?.includes(field.name) ? "✓" : "+" }}</span>
                  <code>{{ field.name }}</code>
                  <small>{{ field.kind }}</small>
                </button>
              </div>
              <div v-else class="node-empty">Подключите источник данных</div>
              <div class="node-label-row step-result-label">
                <span>Результат</span>
                <small v-if="node.previewStats" class="node-ok">{{ node.previewStats.matched_items }} строк</small>
              </div>
              <JsonCodeEditor
                class="node-result-output node-step-preview"
                :model-value="node.preview || (node.previewError ? '' : 'Результат появится здесь')"
                :highlight-syntax="Boolean(node.preview)"
                readonly
                :label="`Результат блока ${node.title}`"
              />
              <p v-if="node.previewError" class="node-error">{{ node.previewError }}</p>
            </template>

            <template v-else-if="node.kind === 'condition'">
              <FilterExpressionEditor
                v-if="node.filterExpression"
                v-model="node.filterExpression"
                :fields="availableConditionFields(node.id)"
                :operators="FILTER_OPERATORS"
              />
              <button
                v-else
                type="button"
                class="add-subblock"
                @click="node.filterExpression = createUiFilterGroup('and', availableConditionFields(node.id)[0]?.name ?? 'state')"
              >+ Создать условие</button>
              <div class="node-label-row step-result-label">
                <span>Результат</span>
                <small v-if="node.previewStats" class="node-ok">
                  {{ node.previewStats.matched_items }} прошло · {{ node.previewStats.filtered_out }} отсеяно
                </small>
              </div>
              <JsonCodeEditor
                class="node-result-output node-step-preview"
                :model-value="node.preview || (node.previewError ? '' : 'Результат появится здесь')"
                :highlight-syntax="Boolean(node.preview)"
                readonly
                :label="`Результат блока ${node.title}`"
              />
              <p v-if="node.previewError" class="node-error">{{ node.previewError }}</p>
            </template>

            <template v-else-if="node.kind === 'template'">
              <div class="converter-settings">
                <label class="node-control compact-control">
                  <span>Шаблон · <code>{value}</code> — значение</span>
                  <input v-model="node.valueTemplate" maxlength="2000" placeholder="0x{value}" />
                </label>
                <label class="converter-option">
                  <input v-model="node.stripOuterQuotes" type="checkbox" />
                  <span>Убирать внешние кавычки</span>
                </label>
              </div>
              <div class="node-label-row step-result-label">
                <span>Массив после преобразования</span>
                <small v-if="node.previewStats" class="node-ok">{{ node.previewStats.values }} значений</small>
              </div>
              <PlainTextViewer
                class="node-step-preview"
                :value="node.preview || (node.previewError ? '' : 'Результат появится здесь')"
                :label="`Результат блока ${node.title}`"
              />
              <p v-if="node.previewError" class="node-error">{{ node.previewError }}</p>
            </template>

            <template v-else-if="node.kind === 'join'">
              <label class="node-control compact-control">
                <span>Разделитель</span>
                <select v-model="node.delimiter" aria-label="Разделитель значений">
                  <option :value="',\n'">Запятая + новая строка</option>
                  <option value=", ">Запятая + пробел</option>
                  <option value=",">Запятая</option>
                  <option :value="'\n'">Новая строка</option>
                </select>
              </label>
              <div class="node-label-row">
                <span>Текст</span>
                <small v-if="node.stats" class="node-ok">{{ node.stats.values }} значений</small>
              </div>
              <PlainTextViewer
                :value="node.output || (node.error ? '' : 'Результат появится здесь')"
                label="Результат блока"
              />
              <p v-if="node.error" class="node-error">{{ node.error }}</p>
            </template>

            <template v-else>
              <label class="node-control compact-control output-format-control">
                <span>Формат</span>
                <select v-model="node.outputFormat" aria-label="Формат результата">
                  <option value="flat">Плоский список</option>
                  <option value="json">JSON</option>
                  <option value="csv">CSV</option>
                  <option value="xml">XML</option>
                  <option value="sql">SQL INSERT</option>
                </select>
              </label>
              <label v-if="node.outputFormat === 'flat'" class="node-control compact-control">
                <span>Разделитель</span>
                <input v-model="node.delimiter" maxlength="12" />
              </label>
              <div v-else-if="node.outputFormat === 'csv'" class="converter-settings">
                <label class="node-control compact-control">
                  <span>Разделитель CSV</span>
                  <input v-model="node.csvDelimiter" maxlength="1" />
                </label>
                <label class="converter-option">
                  <input v-model="node.csvIncludeHeader" type="checkbox" />
                  <span>Добавлять строку заголовков</span>
                </label>
                <label class="converter-option">
                  <input v-model="node.csvQuoteAll" type="checkbox" />
                  <span>Все значения в кавычках</span>
                </label>
              </div>
              <label v-else-if="node.outputFormat === 'sql'" class="node-control compact-control table-control">
                <span>Таблица</span>
                <input v-model="node.tableName" maxlength="64" placeholder="stores" />
              </label>
              <div v-else-if="node.outputFormat === 'xml'" class="xml-controls">
                <label class="node-control compact-control table-control">
                  <span>Корень</span>
                  <input v-model="node.xmlRoot" maxlength="64" placeholder="rows" />
                </label>
                <label class="node-control compact-control table-control">
                  <span>Строка</span>
                  <input v-model="node.xmlRow" maxlength="64" placeholder="row" />
                </label>
              </div>
              <div class="node-label-row">
                <span>Результат</span>
                <small v-if="node.stats" class="node-ok">{{ node.stats.values }} значений</small>
              </div>
              <JsonCodeEditor
                v-if="node.outputFormat === 'json'"
                class="node-result-output"
                :model-value="node.output || (node.error ? '' : 'Результат появится здесь')"
                :highlight-syntax="Boolean(node.output)"
                readonly
                label="Результат блока"
              />
              <PlainTextViewer
                v-else
                :value="node.output || (node.error ? '' : 'Результат появится здесь')"
                label="Результат блока"
              />
              <p v-if="node.error" class="node-error">{{ node.error }}</p>
              <div v-else-if="node.stats" class="output-stats">
                <span><strong>{{ node.stats.matched_items }}</strong> прошло</span>
                <span><strong>{{ node.stats.filtered_out }}</strong> отсеяно</span>
              </div>
            </template>
          </div>

          <footer v-if="outgoingEdges(node.id).length" class="node-connections" @pointerdown.stop>
            <button
              v-for="edge in outgoingEdges(node.id)"
              :key="edge.id"
              type="button"
              :title="`Удалить связь с ${nodeById(edge.to)?.title ?? 'блоком'}`"
              @click="removeEdge(edge.id)"
            >→ {{ nodeById(edge.to)?.title ?? "блок" }} <span>×</span></button>
          </footer>

          <button
            v-if="hasOutputPort(node)"
            type="button"
            class="node-add-next"
            :class="{ active: continuationFor === node.id }"
            :aria-expanded="continuationFor === node.id"
            :aria-label="`Добавить следующий блок после ${node.title}`"
            @pointerdown.stop
            @click.stop="toggleContinuation(node.id)"
          >+</button>

          <div
            v-if="continuationFor === node.id"
            class="continuation-popover"
            :style="{ transform: `scale(${1 / zoom})` }"
            @pointerdown.stop
            @click.stop
          >
            <div class="continuation-heading">
              <span>Продолжить цепочку</span>
              <button type="button" aria-label="Закрыть" @click="closeContinuation">×</button>
            </div>
            <label class="continuation-search">
              <span>⌕</span>
              <input
                v-model="continuationQuery"
                type="search"
                placeholder="Например, SQL или фильтр"
                aria-label="Найти совместимый блок"
                @keydown.enter.prevent="addFirstContinuation(node)"
              />
            </label>
            <div v-if="continuationBlocks(node).length" class="continuation-results">
              <button
                v-for="block in continuationBlocks(node)"
                :key="block.key"
                type="button"
                :style="{ '--node-color': block.color }"
                @click="addContinuation(node, block)"
              >
                <span>{{ block.icon }}</span>
                <div>
                  <strong>{{ block.label }}</strong>
                  <small>{{ block.eyebrow }}</small>
                </div>
                <b>↗</b>
              </button>
            </div>
            <div v-else class="continuation-empty">Совместимых блоков не найдено</div>
          </div>

          <button
            v-if="hasOutputPort(node)"
            type="button"
            class="node-port output-port"
            :class="{ active: connectingFrom === node.id }"
            :data-output-port="node.id"
            :aria-label="`Перетащить связь из блока ${node.title}`"
            @pointerdown.stop="startConnectionDrag($event, node.id)"
          ></button>
        </article>
      </div>

      <aside
        v-if="inspectedNode && inspectorInput && inspectorOutput"
        class="node-inspector"
        :style="{ '--node-color': nodeDisplayMeta(inspectedNode).color }"
        :aria-labelledby="`node-inspector-title-${inspectedNode.id}`"
        @pointerdown.stop
        @wheel.stop
      >
        <header class="node-inspector-heading">
          <span class="node-icon">{{ nodeDisplayMeta(inspectedNode).icon }}</span>
          <div>
            <small>{{ nodeDisplayMeta(inspectedNode).eyebrow }} · вход / выход</small>
            <strong :id="`node-inspector-title-${inspectedNode.id}`">{{ inspectedNode.title }}</strong>
          </div>
          <button type="button" aria-label="Закрыть просмотр входа и выхода" @click="closeNodeInspector">×</button>
        </header>

        <div class="node-inspector-body">
          <section class="node-inspector-pane">
            <div class="node-inspector-pane-heading">
              <div>
                <span>Вход</span>
                <small>{{ inspectorInput.language.toUpperCase() }}</small>
              </div>
              <div class="node-inspector-badges">
                <small v-if="inspectorInput.itemCount !== undefined">{{ inspectorInput.itemCount }} строк</small>
                <small v-if="inspectorInput.truncated" class="is-truncated">первые 50</small>
              </div>
            </div>
            <JsonCodeEditor
              v-if="inspectorInput.content"
              class="node-inspector-code"
              :model-value="inspectorInput.content"
              :language="inspectorInput.language"
              :highlight-syntax="inspectorHighlights(inspectorInput.language)"
              readonly
              :label="`Вход блока ${inspectedNode.title}`"
            />
            <div v-else class="node-inspector-empty" :class="{ 'is-error': inspectorInput.error }">
              {{ inspectorInput.error || inspectorInput.emptyMessage }}
            </div>
          </section>

          <section class="node-inspector-pane">
            <div class="node-inspector-pane-heading">
              <div>
                <span>Выход · результат</span>
                <small>{{ inspectorOutput.language.toUpperCase() }}</small>
              </div>
              <div class="node-inspector-badges">
                <small v-if="inspectorOutput.itemCount !== undefined">{{ inspectorOutput.itemCount }} строк</small>
                <small v-if="inspectorOutput.truncated" class="is-truncated">первые 50</small>
              </div>
            </div>
            <JsonCodeEditor
              v-if="inspectorOutput.content"
              class="node-inspector-code"
              :model-value="inspectorOutput.content"
              :language="inspectorOutput.language"
              :highlight-syntax="inspectorHighlights(inspectorOutput.language)"
              readonly
              :label="`Выход блока ${inspectedNode.title}`"
            />
            <div v-else class="node-inspector-empty" :class="{ 'is-error': inspectorOutput.error }">
              {{ inspectorOutput.error || inspectorOutput.emptyMessage }}
            </div>
          </section>
        </div>

        <footer class="node-inspector-note">
          Промежуточные данные показываются превью до 50 строк; итоговый выход — полностью.
        </footer>
      </aside>

      <div class="canvas-status" aria-live="polite" @pointerdown.stop>
        <span :class="{ pulse: connectingFrom }"></span>
        {{ notice }}
      </div>

      <div class="zoom-controls" aria-label="Масштаб холста" @pointerdown.stop>
        <button type="button" aria-label="Уменьшить" @click="changeZoom(-0.1)">−</button>
        <button type="button" class="zoom-value" @click="fitView">{{ zoomLabel }}</button>
        <button type="button" aria-label="Увеличить" @click="changeZoom(0.1)">+</button>
        <button type="button" class="fit-button" @click="fitView">Вписать</button>
      </div>

      <div class="run-status" @pointerdown.stop>
        <span v-if="isRunning" class="run-spinner"></span>
        <span v-else class="run-check">✓</span>
        <div>
          <strong>{{ isRunning ? "Выполняю граф" : "Граф актуален" }}</strong>
          <small>{{ isRunning
            ? "Проверяю изменённые ветки"
            : `${Math.round(lastRunMs)} мс · ${cachedBranches ? `${cachedBranches} из кэша` : "пересчитано"}`
          }}</small>
        </div>
        <button type="button" title="Сбросить доску" @click="resetGraph">↺</button>
      </div>
    </main>
  </div>
</template>
