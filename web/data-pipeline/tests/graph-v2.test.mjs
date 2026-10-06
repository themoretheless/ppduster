import assert from "node:assert/strict";
import test from "node:test";

import {
  BUILTIN_NODE_REGISTRY,
  preflightConnection,
  stableTopologicalSort,
  validateGraph,
} from "../src/graph-v2/index.ts";

const node = (id, type) => ({
  id,
  type,
  position: { x: 0, y: 0 },
  config: {},
});

const connection = (id, fromNodeId, fromPort, toNodeId, toPort) => ({
  id,
  from: { nodeId: fromNodeId, port: fromPort },
  to: { nodeId: toNodeId, port: toPort },
});

const graph = (nodes, connections = []) => ({
  version: 2,
  id: "test-graph",
  revision: 1,
  nodes,
  connections,
});

const issueCodes = (result) => result.issues.map((issue) => issue.code);

test("Graph v2 validates a complete source-transform-filter-sink chain", () => {
  const document = graph(
    [
      node("source", "source.json"),
      node("project", "transform.project"),
      node("filter", "transform.filter"),
      node("sink", "sink.csv"),
    ],
    [
      connection("c1", "source", "records", "project", "records"),
      connection("c2", "project", "matched", "filter", "records"),
      connection("c3", "filter", "matched", "sink", "records"),
    ],
  );

  assert.deepEqual(validateGraph(document), { valid: true, issues: [] });
  assert.deepEqual(stableTopologicalSort(document), {
    acyclic: true,
    orderedNodeIds: ["source", "project", "filter", "sink"],
    remainingNodeIds: [],
  });
});

test("Graph v2 permits fan-out from a many-cardinality output", () => {
  const document = graph(
    [
      node("source", "source.json"),
      node("project", "transform.project"),
      node("filter", "transform.filter"),
    ],
    [
      connection("to-project", "source", "records", "project", "records"),
      connection("to-filter", "source", "records", "filter", "records"),
    ],
  );

  assert.equal(validateGraph(document).valid, true);
  assert.deepEqual(stableTopologicalSort(document).orderedNodeIds, [
    "source",
    "project",
    "filter",
  ]);
});

test("value-vector transforms remain chainable until an explicit join sink", () => {
  const valid = graph(
    [
      node("source", "source.list"),
      node("template", "transform.template"),
      node("join", "sink.join"),
    ],
    [
      connection("source-template", "source", "values", "template", "values"),
      connection("template-join", "template", "mapped", "join", "values"),
    ],
  );
  assert.equal(validateGraph(valid).valid, true);
  assert.equal(
    BUILTIN_NODE_REGISTRY.get("transform.template").ports.some((port) => port.direction === "output"),
    true,
  );

  const invalid = graph(
    [node("source", "source.list"), node("sink", "sink.csv")],
    [connection("records", "source", "values", "sink", "records")],
  );
  assert.ok(issueCodes(validateGraph(invalid)).includes("incompatible_ports"));
});

test("Graph v2 reports cycles and keeps the cyclic nodes out of the order", () => {
  const document = graph(
    [
      node("source", "source.json"),
      node("first", "transform.project"),
      node("second", "transform.filter"),
    ],
    [
      connection("cycle-a", "first", "matched", "second", "records"),
      connection("cycle-b", "second", "matched", "first", "records"),
    ],
  );

  const validation = validateGraph(document);
  assert.equal(validation.valid, false);
  assert.ok(issueCodes(validation).includes("cycle_detected"));
  assert.deepEqual(stableTopologicalSort(document), {
    acyclic: false,
    orderedNodeIds: ["source"],
    remainingNodeIds: ["first", "second"],
  });
});

test("Graph v2 rejects a second connection to a single-cardinality input", () => {
  const document = graph(
    [
      node("source-a", "source.json"),
      node("source-b", "source.csv"),
      node("project", "transform.project"),
    ],
    [connection("first-input", "source-a", "records", "project", "records")],
  );

  const result = preflightConnection(document, BUILTIN_NODE_REGISTRY, {
    from: { nodeId: "source-b", port: "records" },
    to: { nodeId: "project", port: "records" },
  });

  assert.equal(result.ok, false);
  assert.ok(issueCodes(result).includes("input_cardinality_exceeded"));
});

test("Graph v2 rejects wrong port direction and incompatible data types", async (t) => {
  await t.test("an output cannot be used as the target port", () => {
    const document = graph([
      node("source", "source.json"),
      node("filter", "transform.filter"),
    ]);
    const result = preflightConnection(document, BUILTIN_NODE_REGISTRY, {
      from: { nodeId: "source", port: "records" },
      to: { nodeId: "filter", port: "matched" },
    });

    assert.equal(result.ok, false);
    assert.ok(issueCodes(result).includes("invalid_target_port_direction"));
  });

  await t.test("text output cannot connect to a record-set input", () => {
    const registry = new Map([
      [
        "test.text-source",
        {
          type: "test.text-source",
          version: 1,
          label: "Text source",
          category: "source",
          ports: [
            {
              name: "text",
              label: "Text",
              direction: "output",
              cardinality: "many",
              contract: { kind: "text", formats: ["plain-text"] },
            },
          ],
          defaultConfig: {},
        },
      ],
      [
        "test.record-sink",
        {
          type: "test.record-sink",
          version: 1,
          label: "Record sink",
          category: "sink",
          ports: [
            {
              name: "records",
              label: "Records",
              direction: "input",
              cardinality: "one",
              required: true,
              contract: { kind: "record-set", formats: ["normalized"] },
            },
          ],
          defaultConfig: {},
        },
      ],
    ]);
    const document = graph([
      node("source", "test.text-source"),
      node("sink", "test.record-sink"),
    ]);
    const result = preflightConnection(document, registry, {
      from: { nodeId: "source", port: "text" },
      to: { nodeId: "sink", port: "records" },
    });

    assert.equal(result.ok, false);
    assert.ok(issueCodes(result).includes("incompatible_ports"));
  });
});

test("Graph v2 topological order is stable when connection order changes", () => {
  const nodes = [
    node("source", "source.json"),
    node("left", "transform.project"),
    node("right", "transform.filter"),
    node("sink", "sink.json"),
  ];
  const connections = [
    connection("left", "source", "records", "left", "records"),
    connection("right", "source", "records", "right", "records"),
    connection("left-sink", "left", "matched", "sink", "records"),
    connection("right-sink", "right", "matched", "sink", "records"),
  ];
  const expected = ["source", "left", "right", "sink"];

  assert.deepEqual(
    stableTopologicalSort(graph(nodes, connections)).orderedNodeIds,
    expected,
  );
  assert.deepEqual(
    stableTopologicalSort(graph(nodes, [...connections].reverse())).orderedNodeIds,
    expected,
  );
});
