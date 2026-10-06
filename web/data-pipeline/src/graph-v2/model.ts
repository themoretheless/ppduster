export type NodeId = string;
export type ConnectionId = string;
export type NodeType = string;
export type PortName = string;

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue =
  | JsonPrimitive
  | { readonly [key: string]: JsonValue }
  | readonly JsonValue[];

export interface GraphPosition {
  readonly x: number;
  readonly y: number;
}

export interface GraphNode {
  readonly id: NodeId;
  readonly type: NodeType;
  readonly version?: number;
  readonly position: GraphPosition;
  readonly config: Readonly<Record<string, JsonValue>>;
}

export interface PortEndpoint {
  readonly nodeId: NodeId;
  readonly port: PortName;
}

export interface GraphConnection {
  readonly id: ConnectionId;
  readonly from: PortEndpoint;
  readonly to: PortEndpoint;
}

export interface GraphDocument {
  readonly version: 2;
  readonly id: string;
  readonly name?: string;
  readonly revision: number;
  readonly nodes: readonly GraphNode[];
  readonly connections: readonly GraphConnection[];
}

export type PrimitiveSchemaKind =
  | "string"
  | "number"
  | "integer"
  | "boolean"
  | "null"
  | "unknown";

export interface PrimitiveSchema {
  readonly kind: "primitive";
  readonly type: PrimitiveSchemaKind;
  readonly nullable?: boolean;
}

export interface ObjectFieldSchema {
  readonly name: string;
  readonly schema: ValueSchema;
  readonly optional?: boolean;
}

export interface ObjectSchema {
  readonly kind: "object";
  readonly fields: readonly ObjectFieldSchema[];
  readonly additionalFields: boolean;
}

export interface ArraySchema {
  readonly kind: "array";
  readonly items: ValueSchema;
}

export interface UnionSchema {
  readonly kind: "union";
  readonly variants: readonly ValueSchema[];
}

export type ValueSchema =
  | PrimitiveSchema
  | ObjectSchema
  | ArraySchema
  | UnionSchema;

export interface RecordSetSchema {
  readonly kind: "record-set";
  readonly record: ObjectSchema;
}

export interface TextSchema {
  readonly kind: "text";
  readonly mediaType?: string;
}

export interface ValueVectorSchema {
  readonly kind: "value-vector";
  readonly valueType: PrimitiveSchemaKind;
}

export interface UnknownDataSchema {
  readonly kind: "unknown";
}

export type DataSchema = RecordSetSchema | ValueVectorSchema | TextSchema | UnknownDataSchema;

export type DataKind = "record-set" | "value-vector" | "text" | "unknown";
export type DataFormat =
  | "normalized"
  | "json"
  | "csv"
  | "xml"
  | "sql"
  | "plain-text"
  | "arrow";

export interface PortContract {
  readonly kind: DataKind;
  readonly formats?: readonly DataFormat[];
  readonly schema?: DataSchema;
}

export type PortDirection = "input" | "output";
export type PortCardinality = "one" | "many";

export interface PortDefinition {
  readonly name: PortName;
  readonly label: string;
  readonly direction: PortDirection;
  readonly cardinality: PortCardinality;
  readonly required?: boolean;
  readonly contract: PortContract;
}

export interface NodeDefinition {
  readonly type: NodeType;
  readonly version: number;
  readonly label: string;
  readonly category: "source" | "transform" | "sink";
  readonly ports: readonly PortDefinition[];
  readonly defaultConfig: Readonly<Record<string, JsonValue>>;
}

export type NodeRegistry = ReadonlyMap<NodeType, NodeDefinition>;

export type GraphIssueSeverity = "error" | "warning";

export type GraphIssueCode =
  | "duplicate_node_id"
  | "duplicate_connection_id"
  | "unknown_node_type"
  | "unknown_source_node"
  | "unknown_target_node"
  | "unknown_source_port"
  | "unknown_target_port"
  | "invalid_source_port_direction"
  | "invalid_target_port_direction"
  | "incompatible_ports"
  | "input_cardinality_exceeded"
  | "self_connection"
  | "duplicate_connection"
  | "required_input_missing"
  | "invalid_node_config"
  | "cycle_detected";

export interface GraphIssue {
  readonly code: GraphIssueCode;
  readonly severity: GraphIssueSeverity;
  readonly message: string;
  readonly nodeId?: NodeId;
  readonly connectionId?: ConnectionId;
  readonly portName?: PortName;
}

export interface GraphValidationResult {
  readonly valid: boolean;
  readonly issues: readonly GraphIssue[];
}
