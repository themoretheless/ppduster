import type {
  ExecutePlanFieldSchema,
  ExecutePlanDataSchema,
  ExecutePlanRecordSchema,
  ExecutePlanRequest,
  ExecutePlanResponse,
  ExecutePlanStep,
} from "./execute-plan.ts";
import { conditionsInFilterExpression } from "./filter-expression.ts";

export type SchemaConfigDiagnosticCode =
  | "selected_field_not_in_input_schema"
  | "condition_field_not_in_input_schema";

export interface SchemaConfigDiagnostic {
  readonly severity: "warning";
  readonly code: SchemaConfigDiagnosticCode;
  readonly message: string;
  readonly node_id: string;
  readonly field: string;
  /** Stable path into the normalized execute-plan step configuration. */
  readonly config_path: string;
}

/** Schema snapshot used by editors for one concrete execute-plan step. */
export interface ExecutePlanNodeSchemaState {
  readonly node_id: string;
  readonly node_type: ExecutePlanStep["node_type"];
  readonly input_node_id: string | null;
  readonly input_port: string | null;
  /** The actual output schema of the step's immediate parent. */
  readonly input_schema: ExecutePlanRecordSchema | null;
  /** The schema emitted by this node in the corresponding execution. */
  readonly output_schema: ExecutePlanRecordSchema | null;
  readonly diagnostics: readonly SchemaConfigDiagnostic[];
}

export interface ExecutePlanSchemaState {
  readonly nodes: Readonly<Record<string, ExecutePlanNodeSchemaState>>;
  readonly diagnostics: readonly SchemaConfigDiagnostic[];
}

/** UI-facing schema-aware response produced by buildExecutePlanSchemaState. */
export type SchemaAwareExecutePlanResponse = ExecutePlanSchemaState;
export type ExecutePlanSchemaResponse = ExecutePlanResponse | SchemaAwareExecutePlanResponse;

type ExecutePlan = ExecutePlanRequest["plan"];

const recordSchema = (schema: ExecutePlanDataSchema | null | undefined): ExecutePlanRecordSchema | null =>
  schema?.kind === "records" ? schema : null;

const stepById = (plan: ExecutePlan, nodeId: string): ExecutePlanStep | undefined =>
  plan.steps.find((step) => step.node_id === nodeId);

/**
 * Resolves a node's input from its direct binding. It never walks ancestors and
 * therefore cannot accidentally reintroduce fields removed by an intermediate
 * projection.
 */
export const getNodeInputSchema = (
  plan: ExecutePlan,
  response: ExecutePlanResponse,
  nodeId: string,
): ExecutePlanRecordSchema | null => {
  const step = stepById(plan, nodeId);
  if (!step?.input) return null;
  const ownInputSchema = response.nodes[nodeId]?.input_schema;
  if (ownInputSchema !== undefined) return recordSchema(ownInputSchema);
  const parent = response.nodes[step.input.node_id];
  return recordSchema(parent?.output_schema ?? parent?.schema);
};

export const getNodeOutputSchema = (
  response: ExecutePlanResponse,
  nodeId: string,
): ExecutePlanRecordSchema | null =>
  recordSchema(response.nodes[nodeId]?.output_schema ?? response.nodes[nodeId]?.schema);

/** Fields suitable for a dropdown on this exact node input. */
export const getActualInputFields = (
  plan: ExecutePlan,
  response: ExecutePlanResponse,
  nodeId: string,
): readonly ExecutePlanFieldSchema[] =>
  getNodeInputSchema(plan, response, nodeId)?.fields ?? [];

/** Actual fields offered by a node's input editor. */
export const inputFieldsForNode = (
  response: ExecutePlanSchemaResponse,
  nodeId: string,
): readonly ExecutePlanFieldSchema[] =>
  recordSchema(response.nodes[nodeId]?.input_schema)?.fields ?? [];

/** Fields physically emitted by the node after its transformation. */
export const outputFieldsForNode = (
  response: ExecutePlanSchemaResponse,
  nodeId: string,
): readonly ExecutePlanFieldSchema[] => {
  const node = response.nodes[nodeId];
  if (!node) return [];
  const output = recordSchema(node.output_schema);
  if (output) return output.fields;
  return "schema" in node ? recordSchema(node.schema)?.fields ?? [] : [];
};

/** Selected names which are no longer present on this node's direct input. */
export const staleFieldNames = (
  response: ExecutePlanSchemaResponse,
  nodeId: string,
  selected: readonly string[],
): readonly string[] => {
  const inputSchema = recordSchema(response.nodes[nodeId]?.input_schema);
  // No schema means "not executed/failed upstream", not "known to have zero fields".
  if (!inputSchema) return [];
  const available = new Set(inputSchema.fields.map((field) => field.name));
  return selected.filter((field) => !available.has(field));
};

const staleField = (
  nodeId: string,
  field: string,
  configPath: string,
): SchemaConfigDiagnostic => ({
  severity: "warning",
  code: "selected_field_not_in_input_schema",
  message: `Поле «${field}» больше не существует во входе этого блока`,
  node_id: nodeId,
  field,
  config_path: configPath,
});

const staleCondition = (
  nodeId: string,
  field: string,
  configPath: string,
): SchemaConfigDiagnostic => ({
  severity: "warning",
  code: "condition_field_not_in_input_schema",
  message: `Условие использует поле «${field}», которого больше нет во входе этого блока`,
  node_id: nodeId,
  field,
  config_path: configPath,
});

const rootFieldName = (path: string): string | null => {
  if (path === "$" || path.startsWith("$[")) return null;
  const normalized = path.startsWith("$.") ? path.slice(2) : path;
  const bracket = normalized.indexOf("[");
  const dot = normalized.indexOf(".");
  const end = [bracket, dot].filter((index) => index >= 0).reduce(
    (smallest, index) => Math.min(smallest, index),
    normalized.length,
  );
  return normalized.slice(0, end);
};

/**
 * Diagnoses settings against a known immediate input schema. A missing schema
 * means upstream execution failed or has not run yet; in that case no field is
 * labelled stale based on incomplete evidence.
 */
export const diagnoseStaleNodeFields = (
  step: ExecutePlanStep,
  inputSchema: ExecutePlanRecordSchema | null,
): readonly SchemaConfigDiagnostic[] => {
  if (!inputSchema || step.node_type === "source") return [];
  const available = new Set(inputSchema.fields.map((field) => field.name));

  if (step.node_type === "filter") {
    const conditions = step.config.expression
      ? conditionsInFilterExpression(step.config.expression).map(({ condition, path }) => ({
          condition,
          path: `${path}.field`,
        }))
      : step.config.filters.map((condition, index) => ({
          condition,
          path: `filters[${index}].field`,
        }));
    return conditions.flatMap(({ condition, path }) => {
      const rootField = rootFieldName(condition.field);
      return rootField === null || available.has(rootField)
        ? []
        : [staleCondition(step.node_id, condition.field, path)];
    });
  }

  if (step.node_type === "template") return [];

  const selected = step.node_type === "project"
    ? step.config.fields
    : (step.config.fields ?? []);
  return selected.flatMap((field, index) =>
    available.has(field)
      ? []
      : [staleField(step.node_id, field, `fields[${index}]`)],
  );
};

/** Adapts one execute-plan response into UI-friendly per-node schema state. */
export const buildExecutePlanSchemaState = (
  request: ExecutePlanRequest,
  response: ExecutePlanResponse,
): ExecutePlanSchemaState => {
  const nodes: Record<string, ExecutePlanNodeSchemaState> = {};
  const diagnostics: SchemaConfigDiagnostic[] = [];

  for (const step of request.plan.steps) {
    const inputSchema = getNodeInputSchema(request.plan, response, step.node_id);
    const nodeDiagnostics = diagnoseStaleNodeFields(step, inputSchema);
    diagnostics.push(...nodeDiagnostics);
    nodes[step.node_id] = {
      node_id: step.node_id,
      node_type: step.node_type,
      input_node_id: step.input?.node_id ?? null,
      input_port: step.input?.port ?? null,
      input_schema: inputSchema,
      output_schema: getNodeOutputSchema(response, step.node_id),
      diagnostics: nodeDiagnostics,
    };
  }

  return { nodes, diagnostics };
};
