import type {
  GraphConnection,
  GraphDocument,
  GraphNode,
  NodeDefinition,
  NodeId,
  NodeRegistry,
  PortDefinition,
  PortEndpoint,
} from "./model.ts";
import { BUILTIN_NODE_REGISTRY } from "./builtins.ts";

export interface GraphIndex {
  readonly nodesById: ReadonlyMap<NodeId, GraphNode>;
  readonly definitionsByNodeId: ReadonlyMap<NodeId, NodeDefinition>;
  readonly incomingByNodeId: ReadonlyMap<NodeId, readonly GraphConnection[]>;
  readonly outgoingByNodeId: ReadonlyMap<NodeId, readonly GraphConnection[]>;
  readonly incomingByEndpoint: ReadonlyMap<string, readonly GraphConnection[]>;
  readonly outgoingByEndpoint: ReadonlyMap<string, readonly GraphConnection[]>;
}

export const endpointKey = (endpoint: PortEndpoint): string =>
  `${endpoint.nodeId}\u0000${endpoint.port}`;

const append = <T>(map: Map<string, T[]>, key: string, value: T): void => {
  const values = map.get(key);
  if (values) {
    values.push(value);
  } else {
    map.set(key, [value]);
  }
};

export const findPort = (
  definition: NodeDefinition | undefined,
  name: string,
): PortDefinition | undefined =>
  definition?.ports.find((port) => port.name === name);

export const createGraphIndex = (
  graph: GraphDocument,
  registry: NodeRegistry = BUILTIN_NODE_REGISTRY,
): GraphIndex => {
  const nodesById = new Map<NodeId, GraphNode>();
  const definitionsByNodeId = new Map<NodeId, NodeDefinition>();
  const incomingByNodeId = new Map<string, GraphConnection[]>();
  const outgoingByNodeId = new Map<string, GraphConnection[]>();
  const incomingByEndpoint = new Map<string, GraphConnection[]>();
  const outgoingByEndpoint = new Map<string, GraphConnection[]>();

  for (const node of graph.nodes) {
    if (!nodesById.has(node.id)) nodesById.set(node.id, node);
    const definition = registry.get(node.type);
    if (definition && !definitionsByNodeId.has(node.id)) {
      definitionsByNodeId.set(node.id, definition);
    }
  }

  for (const connection of graph.connections) {
    append(outgoingByNodeId, connection.from.nodeId, connection);
    append(incomingByNodeId, connection.to.nodeId, connection);
    append(outgoingByEndpoint, endpointKey(connection.from), connection);
    append(incomingByEndpoint, endpointKey(connection.to), connection);
  }

  return {
    nodesById,
    definitionsByNodeId,
    incomingByNodeId,
    outgoingByNodeId,
    incomingByEndpoint,
    outgoingByEndpoint,
  };
};
