import { BUILTIN_NODE_REGISTRY } from "./builtins.ts";
import { createGraphIndex, endpointKey, findPort } from "./indexes.ts";
import type {
  GraphConnection,
  GraphDocument,
  GraphIssue,
  GraphValidationResult,
  NodeId,
  NodeRegistry,
  PortContract,
} from "./model.ts";

export interface TopologicalSortResult {
  readonly acyclic: boolean;
  readonly orderedNodeIds: readonly NodeId[];
  readonly remainingNodeIds: readonly NodeId[];
}

export interface ConnectionCandidate {
  readonly id?: string;
  readonly from: GraphConnection["from"];
  readonly to: GraphConnection["to"];
}

export interface ConnectionPreflightResult {
  readonly ok: boolean;
  readonly issues: readonly GraphIssue[];
}

const issue = (
  code: GraphIssue["code"],
  message: string,
  context: Omit<GraphIssue, "code" | "severity" | "message"> = {},
): GraphIssue => ({ code, severity: "error", message, ...context });

export const arePortContractsCompatible = (
  source: PortContract,
  target: PortContract,
): boolean => {
  if (source.kind === "unknown" || target.kind === "unknown") return true;
  if (source.kind !== target.kind) return false;
  if (!target.formats?.length || !source.formats?.length) return true;
  return source.formats.some((format) => target.formats?.includes(format));
};

export const stableTopologicalSort = (
  graph: Pick<GraphDocument, "nodes" | "connections">,
): TopologicalSortResult => {
  const order = new Map<NodeId, number>();
  const indegree = new Map<NodeId, number>();
  const outgoing = new Map<NodeId, NodeId[]>();

  graph.nodes.forEach((node, index) => {
    if (!order.has(node.id)) order.set(node.id, index);
    if (!indegree.has(node.id)) indegree.set(node.id, 0);
  });

  const seenEdges = new Set<string>();
  for (const connection of graph.connections) {
    if (!indegree.has(connection.from.nodeId) || !indegree.has(connection.to.nodeId)) {
      continue;
    }
    const edgeKey = `${connection.from.nodeId}\u0000${connection.to.nodeId}`;
    if (seenEdges.has(edgeKey)) continue;
    seenEdges.add(edgeKey);
    indegree.set(connection.to.nodeId, (indegree.get(connection.to.nodeId) ?? 0) + 1);
    const targets = outgoing.get(connection.from.nodeId);
    if (targets) targets.push(connection.to.nodeId);
    else outgoing.set(connection.from.nodeId, [connection.to.nodeId]);
  }

  const ready = graph.nodes
    .filter((node, index) => order.get(node.id) === index && indegree.get(node.id) === 0)
    .map((node) => node.id);
  const orderedNodeIds: NodeId[] = [];

  while (ready.length > 0) {
    const nodeId = ready.shift();
    if (nodeId === undefined) break;
    orderedNodeIds.push(nodeId);
    for (const targetId of outgoing.get(nodeId) ?? []) {
      const next = (indegree.get(targetId) ?? 0) - 1;
      indegree.set(targetId, next);
      if (next === 0) {
        const targetOrder = order.get(targetId) ?? Number.MAX_SAFE_INTEGER;
        const insertionIndex = ready.findIndex(
          (readyId) => (order.get(readyId) ?? Number.MAX_SAFE_INTEGER) > targetOrder,
        );
        if (insertionIndex < 0) ready.push(targetId);
        else ready.splice(insertionIndex, 0, targetId);
      }
    }
  }

  const emitted = new Set(orderedNodeIds);
  const remainingNodeIds = graph.nodes
    .map((node) => node.id)
    .filter((nodeId, index, all) => all.indexOf(nodeId) === index && !emitted.has(nodeId));

  return {
    acyclic: remainingNodeIds.length === 0,
    orderedNodeIds,
    remainingNodeIds,
  };
};

const validateCandidate = (
  graph: GraphDocument,
  registry: NodeRegistry,
  candidate: ConnectionCandidate,
): GraphIssue[] => {
  const issues: GraphIssue[] = [];
  const index = createGraphIndex(graph, registry);
  const sourceNode = index.nodesById.get(candidate.from.nodeId);
  const targetNode = index.nodesById.get(candidate.to.nodeId);

  if (!sourceNode) {
    issues.push(issue("unknown_source_node", "Исходный блок не найден", { nodeId: candidate.from.nodeId }));
  }
  if (!targetNode) {
    issues.push(issue("unknown_target_node", "Целевой блок не найден", { nodeId: candidate.to.nodeId }));
  }
  if (!sourceNode || !targetNode) return issues;

  if (sourceNode.id === targetNode.id) {
    issues.push(issue("self_connection", "Блок нельзя соединить с самим собой", { nodeId: sourceNode.id }));
  }

  const sourceDefinition = registry.get(sourceNode.type);
  const targetDefinition = registry.get(targetNode.type);
  const sourcePort = findPort(sourceDefinition, candidate.from.port);
  const targetPort = findPort(targetDefinition, candidate.to.port);

  if (!sourcePort) {
    issues.push(issue("unknown_source_port", `Выход «${candidate.from.port}» не найден`, {
      nodeId: sourceNode.id,
      portName: candidate.from.port,
    }));
  } else if (sourcePort.direction !== "output") {
    issues.push(issue("invalid_source_port_direction", `Порт «${candidate.from.port}» не является выходом`, {
      nodeId: sourceNode.id,
      portName: candidate.from.port,
    }));
  }

  if (!targetPort) {
    issues.push(issue("unknown_target_port", `Вход «${candidate.to.port}» не найден`, {
      nodeId: targetNode.id,
      portName: candidate.to.port,
    }));
  } else if (targetPort.direction !== "input") {
    issues.push(issue("invalid_target_port_direction", `Порт «${candidate.to.port}» не является входом`, {
      nodeId: targetNode.id,
      portName: candidate.to.port,
    }));
  }

  if (
    sourcePort?.direction === "output" &&
    targetPort?.direction === "input" &&
    !arePortContractsCompatible(sourcePort.contract, targetPort.contract)
  ) {
    issues.push(issue("incompatible_ports", "Тип данных выхода несовместим с типом входа", {
      nodeId: targetNode.id,
      portName: targetPort.name,
    }));
  }

  const duplicate = graph.connections.some(
    (connection) =>
      connection.from.nodeId === candidate.from.nodeId &&
      connection.from.port === candidate.from.port &&
      connection.to.nodeId === candidate.to.nodeId &&
      connection.to.port === candidate.to.port,
  );
  if (duplicate) {
    issues.push(issue("duplicate_connection", "Такое соединение уже существует"));
  }

  if (
    targetPort?.cardinality === "one" &&
    (index.incomingByEndpoint.get(endpointKey(candidate.to))?.length ?? 0) > 0
  ) {
    issues.push(issue("input_cardinality_exceeded", `Вход «${targetPort.name}» уже подключён`, {
      nodeId: targetNode.id,
      portName: targetPort.name,
    }));
  }

  if (issues.length === 0) {
    const synthetic: GraphConnection = {
      id: candidate.id ?? "__preflight__",
      from: candidate.from,
      to: candidate.to,
    };
    const topology = stableTopologicalSort({
      nodes: graph.nodes,
      connections: [...graph.connections, synthetic],
    });
    if (!topology.acyclic) {
      issues.push(issue("cycle_detected", "Соединение создаёт цикл"));
    }
  }

  return issues;
};

export const preflightConnection = (
  graph: GraphDocument,
  registry: NodeRegistry,
  candidate: ConnectionCandidate,
): ConnectionPreflightResult => {
  const issues = validateCandidate(graph, registry, candidate);
  return { ok: issues.length === 0, issues };
};

export const validateGraph = (
  graph: GraphDocument,
  registry: NodeRegistry = BUILTIN_NODE_REGISTRY,
): GraphValidationResult => {
  const issues: GraphIssue[] = [];
  const nodeIds = new Set<NodeId>();
  const connectionIds = new Set<string>();

  for (const node of graph.nodes) {
    if (nodeIds.has(node.id)) {
      issues.push(issue("duplicate_node_id", `Повторяющийся идентификатор блока «${node.id}»`, { nodeId: node.id }));
    } else {
      nodeIds.add(node.id);
    }
    if (!registry.has(node.type)) {
      issues.push(issue("unknown_node_type", `Неизвестный тип блока «${node.type}»`, { nodeId: node.id }));
    }
  }

  for (const connection of graph.connections) {
    if (connectionIds.has(connection.id)) {
      issues.push(issue("duplicate_connection_id", `Повторяющийся идентификатор связи «${connection.id}»`, {
        connectionId: connection.id,
      }));
    } else {
      connectionIds.add(connection.id);
    }
    issues.push(...validateCandidate(
      { ...graph, connections: graph.connections.filter((item) => item.id !== connection.id) },
      registry,
      connection,
    ).map((item) => ({ ...item, connectionId: connection.id })));
  }

  const index = createGraphIndex(graph, registry);
  for (const node of graph.nodes) {
    const definition = registry.get(node.type);
    for (const port of definition?.ports ?? []) {
      if (
        port.direction === "input" &&
        port.required &&
        (index.incomingByEndpoint.get(endpointKey({ nodeId: node.id, port: port.name }))?.length ?? 0) === 0
      ) {
        issues.push(issue("required_input_missing", `Обязательный вход «${port.name}» не подключён`, {
          nodeId: node.id,
          portName: port.name,
        }));
      }
    }
  }

  const topology = stableTopologicalSort(graph);
  if (!topology.acyclic) {
    issues.push(issue("cycle_detected", "Граф содержит цикл", { nodeId: topology.remainingNodeIds[0] }));
  }

  return { valid: !issues.some((item) => item.severity === "error"), issues };
};
