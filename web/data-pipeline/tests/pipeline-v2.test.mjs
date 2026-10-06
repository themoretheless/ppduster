import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_PIPELINE_BYTES,
  MAX_PIPELINE_CONNECTIONS,
  MAX_PIPELINE_NODES,
  PipelineCodecError,
  decodePipelineV2,
  encodePipelineV2,
} from "../src/pipeline/model.ts";
import { migratePipelineFile } from "../src/pipeline/migrate.ts";

const condition = (field, value) => ({
  kind: "condition",
  field,
  operator: "equal",
  value,
});

const nestedExpression = {
  kind: "group",
  operator: "and",
  children: [
    condition("state", "1"),
    {
      kind: "not",
      child: {
        kind: "group",
        operator: "or",
        children: [condition("locality", "Архив"), condition("name", "Удалено")],
      },
    },
  ],
};

const v2Pipeline = () => ({
  format: "peregon-pipeline",
  version: 2,
  savedAt: "2026-07-31T12:00:00.000Z",
  metadata: { name: "Stores export", description: "Contract fixture" },
  view: { x: 120, y: -40, zoom: 0.85, selectedNodeIds: ["filter"] },
  graph: {
    version: 2,
    id: "stores-pipeline",
    name: "Stores",
    revision: 9,
    nodes: [
      {
        id: "source",
        type: "source.json",
        version: 1,
        position: { x: 0, y: 0 },
        config: { text: '{"stores":[]}', arrayPath: "/stores" },
      },
      {
        id: "project",
        type: "transform.project",
        version: 1,
        position: { x: 300, y: 0 },
        config: { fields: ["id", "name"] },
      },
      {
        id: "filter",
        type: "transform.filter",
        version: 1,
        position: { x: 600, y: -100 },
        config: { expression: nestedExpression },
      },
      {
        id: "csv",
        type: "sink.csv",
        version: 1,
        position: { x: 600, y: 180 },
        config: { delimiter: ";", includeHeader: true },
      },
    ],
    connections: [
      {
        id: "source-project",
        from: { nodeId: "source", port: "records" },
        to: { nodeId: "project", port: "records" },
      },
      {
        id: "project-filter",
        from: { nodeId: "project", port: "matched" },
        to: { nodeId: "filter", port: "records" },
      },
      {
        id: "project-csv",
        from: { nodeId: "project", port: "matched" },
        to: { nodeId: "csv", port: "records" },
      },
    ],
  },
});

test("v2 pipeline roundtrip is stable and contains no ephemeral execution state", () => {
  const original = v2Pipeline();
  const encoded = encodePipelineV2(original, { pretty: true });
  const decoded = decodePipelineV2(encoded);

  assert.deepEqual(decoded, original);
  for (const forbidden of ["preview", "stats", "cache", "runtime", "output_schema"]) {
    assert.equal(encoded.includes(`\"${forbidden}\"`), false, forbidden);
  }
  assert.equal(encodePipelineV2(decoded, { pretty: true }), encoded);
});

test("v1 pipeline migrates to graph v2 with typed node configs and named ports", () => {
  const migrated = migratePipelineFile({
    format: "json-rivet-pipeline",
    version: 1,
    savedAt: "2025-01-02T03:04:05.000Z",
    view: { panX: 12, panY: -8, zoom: 0.7 },
    nodes: [
      {
        id: "source",
        kind: "source",
        title: "Данные",
        x: 0,
        y: 0,
        sourceFormat: "json",
        json: '{"stores":[]}',
      },
      {
        id: "fields",
        kind: "fields",
        title: "Поля",
        x: 300,
        y: 0,
        selectedPath: "/stores",
        selectedFields: ["id", "name", "state"],
      },
      {
        id: "filter",
        kind: "condition",
        title: "Активные",
        x: 600,
        y: 0,
        filterMode: "all",
        conditions: [{ id: 1, field: "state", operator: "equal", value: "1" }],
      },
      {
        id: "output",
        kind: "output",
        title: "CSV",
        x: 900,
        y: 0,
        outputFormat: "csv",
        csvDelimiter: ";",
        csvIncludeHeader: true,
      },
    ],
    edges: [
      { id: "e1", from: "source", to: "fields" },
      { id: "e2", from: "fields", to: "filter" },
      { id: "e3", from: "filter", to: "output" },
    ],
  });

  assert.equal(migrated.version, 2);
  assert.equal(migrated.format, "peregon-pipeline");
  assert.deepEqual(migrated.view, { x: 12, y: -8, zoom: 0.7 });
  assert.deepEqual(migrated.graph.nodes.map((node) => node.type), [
    "source.json",
    "transform.project",
    "transform.filter",
    "sink.csv",
  ]);
  assert.deepEqual(migrated.graph.nodes[1].config.fields, ["id", "name", "state"]);
  assert.deepEqual(migrated.graph.nodes[2].config.expression, condition("state", "1"));
  assert.deepEqual(
    migrated.graph.connections.map((edge) => [edge.from.port, edge.to.port]),
    [["records", "records"], ["matched", "records"], ["matched", "records"]],
  );
});

test("nested filter AST survives v2 decoding and encoding without flattening", () => {
  const pipeline = decodePipelineV2(encodePipelineV2(v2Pipeline()));
  const filter = pipeline.graph.nodes.find((node) => node.id === "filter");

  assert.deepEqual(filter.config.expression, nestedExpression);
  assert.equal(filter.config.expression.children[1].kind, "not");
  assert.equal(filter.config.expression.children[1].child.kind, "group");
});

test("value-vector template and join nodes survive v2 migration with named ports", () => {
  const value = v2Pipeline();
  value.graph.nodes = [
    {
      id: "source",
      type: "source.list",
      position: { x: 0, y: 0 },
      config: { text: '"A",\n"B"' },
    },
    {
      id: "template",
      type: "transform.template",
      position: { x: 300, y: 0 },
      config: { template: "0x{value}", stripOuterQuotes: true },
    },
    {
      id: "join",
      type: "sink.join",
      position: { x: 600, y: 0 },
      config: { delimiter: ",\n" },
    },
  ];
  value.graph.connections = [
    {
      id: "source-template",
      from: { nodeId: "source", port: "values" },
      to: { nodeId: "template", port: "values" },
    },
    {
      id: "template-join",
      from: { nodeId: "template", port: "mapped" },
      to: { nodeId: "join", port: "values" },
    },
  ];

  const migrated = migratePipelineFile(value);
  assert.deepEqual(migrated.graph.nodes.map((item) => item.type), [
    "source.list",
    "transform.template",
    "sink.join",
  ]);
  assert.deepEqual(
    migrated.graph.connections.map((edge) => [edge.from.port, edge.to.port]),
    [["values", "values"], ["mapped", "values"]],
  );
});

test("named ports and branching survive the pipeline roundtrip", () => {
  const pipeline = decodePipelineV2(encodePipelineV2(v2Pipeline()));

  assert.deepEqual(pipeline.graph.connections, v2Pipeline().graph.connections);
  assert.equal(
    pipeline.graph.connections.filter((edge) => edge.from.nodeId === "project").length,
    2,
  );
  assert.deepEqual(
    pipeline.graph.connections
      .filter((edge) => edge.from.nodeId === "project")
      .map((edge) => edge.from.port),
    ["matched", "matched"],
  );
});

test("malformed, unsupported, ephemeral, and oversized files are rejected", async (t) => {
  const rejects = (value) => assert.throws(
    () => decodePipelineV2(value),
    (error) => error instanceof PipelineCodecError,
  );

  await t.test("malformed envelope", () => rejects({ format: "peregon-pipeline", version: 2 }));
  await t.test("unsupported version", () => rejects({ ...v2Pipeline(), version: 99 }));
  await t.test("recursive ephemeral state", () => {
    for (const key of ["output", "preview", "stats", "error", "cache", "runtime"]) {
      const value = v2Pipeline();
      value.graph.nodes[0].config[key] = { secret: "runtime result" };
      rejects(value);
    }
  });
  await t.test("too many nodes", () => {
    const value = v2Pipeline();
    value.graph.nodes = Array.from({ length: MAX_PIPELINE_NODES + 1 }, (_, index) => ({
      id: `source-${index}`,
      type: "source.json",
      position: { x: index, y: 0 },
      config: { text: "[]" },
    }));
    value.graph.connections = [];
    rejects(value);
  });
  await t.test("too many connections", () => {
    const value = v2Pipeline();
    value.graph.connections = Array.from(
      { length: MAX_PIPELINE_CONNECTIONS + 1 },
      (_, index) => ({
        id: `connection-${index}`,
        from: { nodeId: "source", port: "records" },
        to: { nodeId: "project", port: "records" },
      }),
    );
    rejects(value);
  });
  await t.test("byte input above file-size limit", () => {
    rejects(new Uint8Array(MAX_PIPELINE_BYTES + 1));
  });
});
