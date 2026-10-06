/** Wire-format version for messages exchanged with the graph runtime worker. */
export const RUNTIME_CONTRACT_VERSION = 1 as const;

export type RuntimeContractVersion = typeof RUNTIME_CONTRACT_VERSION;

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type RuntimeValueKind =
  | "records"
  | "object"
  | "array"
  | "scalar"
  | "text"
  | "binary"
  | "table";

export type RuntimeFieldKind =
  | "string"
  | "number"
  | "boolean"
  | "object"
  | "array"
  | "null"
  | "mixed"
  | "unknown";

export interface RuntimeSchemaField {
  /** JSON-pointer-like path relative to one record, for example `/address/city`. */
  path: string;
  name: string;
  kind: RuntimeFieldKind;
  nullable: boolean;
  optional: boolean;
  children?: RuntimeSchemaField[];
}

export interface RuntimeSchema {
  kind: RuntimeValueKind;
  fields: RuntimeSchemaField[];
  /** Incremented when a node intentionally changes the shape of its output. */
  revision: number;
}

export type DiagnosticSeverity = "info" | "warning" | "error";

export interface RuntimeDiagnostic {
  code: string;
  severity: DiagnosticSeverity;
  message: string;
  nodeId?: string;
  connectionId?: string;
  portId?: string;
  fieldPath?: string;
  details?: JsonValue;
}

export interface GraphNodeSpec {
  id: string;
  type: string;
  /** Version of the node definition/configuration, independent of this protocol. */
  version?: number;
  position: { x: number; y: number };
  config: Readonly<Record<string, JsonValue>>;
}

export interface GraphConnectionSpec {
  id: string;
  from: { nodeId: string; port: string };
  to: { nodeId: string; port: string };
}

/** Serializable v2 graph snapshot accepted by a compiler. */
export interface GraphDefinition {
  version: 2;
  id: string;
  name?: string;
  revision: number;
  nodes: GraphNodeSpec[];
  connections: GraphConnectionSpec[];
}

export interface NodeInputBinding {
  targetPortId: string;
  sourceNodeId: string;
  sourcePortId: string;
  connectionId: string;
}

export interface NodeOutputPort {
  portId: string;
  kind: RuntimeValueKind;
}

export interface NodeCachePolicy {
  mode: "disabled" | "content";
  /** Included in the cache key so node implementation changes invalidate old data. */
  implementationVersion: string;
}

/** One normalized, executable operation produced by graph compilation. */
export interface NodeStep {
  nodeId: string;
  nodeType: string;
  nodeVersion: number;
  config: Readonly<Record<string, JsonValue>>;
  inputs: NodeInputBinding[];
  outputs: NodeOutputPort[];
  /** Explicit dependencies in stable execution order. */
  dependsOn: string[];
  cache: NodeCachePolicy;
}

export interface ExecutionPlan {
  contractVersion: RuntimeContractVersion;
  planId: string;
  graphId: string;
  graphRevision: number;
  /** Topologically sorted; equal graphs must compile to the same order. */
  steps: NodeStep[];
  entryNodeIds: string[];
  outputNodeIds: string[];
}

export interface CompileResult {
  plan?: ExecutionPlan;
  diagnostics: RuntimeDiagnostic[];
}

export interface RuntimeStats {
  inputItems: number;
  outputItems: number;
  skippedItems: number;
  errorItems: number;
  durationMs: number;
  cacheHit: boolean;
}

export interface RuntimePreview {
  /** A bounded sample only; complete data remains in the runtime dataset store. */
  items: JsonValue[];
  totalItems: number;
  truncated: boolean;
}

export interface RuntimeDatasetRef {
  datasetId: string;
  kind: RuntimeValueKind;
  contentHash: string;
}

export interface NodeOutputResult {
  portId: string;
  dataset?: RuntimeDatasetRef;
  schema?: RuntimeSchema;
  preview?: RuntimePreview;
}

export type NodeExecutionStatus = "completed" | "cached" | "skipped" | "failed";

/** Durable result of one step. It is also suitable for incremental UI updates. */
export interface NodeResult {
  nodeId: string;
  status: NodeExecutionStatus;
  outputs: NodeOutputResult[];
  stats: RuntimeStats;
  diagnostics: RuntimeDiagnostic[];
}

export interface ExecutionResult {
  executionId: string;
  planId: string;
  status: "completed" | "cancelled" | "failed";
  nodes: NodeResult[];
  diagnostics: RuntimeDiagnostic[];
}

interface WorkerEnvelopeBase {
  contractVersion: RuntimeContractVersion;
  requestId: string;
}

export type RuntimeWorkerRequest =
  | (WorkerEnvelopeBase & {
      type: "compile";
      payload: { graph: GraphDefinition };
    })
  | (WorkerEnvelopeBase & {
      type: "execute";
      payload: { plan: ExecutionPlan; previewLimit?: number };
    })
  | (WorkerEnvelopeBase & {
      type: "cancel";
      payload: { executionId: string };
    });

export type RuntimeWorkerReply =
  | (WorkerEnvelopeBase & {
      type: "compile.result";
      payload: CompileResult;
    })
  | (WorkerEnvelopeBase & {
      type: "execute.started";
      payload: { executionId: string; planId: string };
    })
  | (WorkerEnvelopeBase & {
      type: "execute.node-result";
      payload: { executionId: string; result: NodeResult };
    })
  | (WorkerEnvelopeBase & {
      type: "execute.result";
      payload: ExecutionResult;
    })
  | (WorkerEnvelopeBase & {
      type: "request.failed";
      payload: { diagnostics: RuntimeDiagnostic[] };
    });
