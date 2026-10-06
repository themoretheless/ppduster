import assert from "node:assert/strict";
import test from "node:test";

import {
  ExecutionPlanCompileError,
  buildExecutionPlanRequest,
} from "../src/runtime/index.ts";

const node = (id, type, config = {}) => ({
  id,
  type,
  position: { x: 0, y: 0 },
  config,
});

const connection = (id, fromNodeId, fromPort, toNodeId, toPort = "records") => ({
  id,
  from: { nodeId: fromNodeId, port: fromPort },
  to: { nodeId: toNodeId, port: toPort },
});

const graph = (nodes, connections, overrides = {}) => ({
  version: 2,
  id: "execution-contract",
  revision: 7,
  nodes,
  connections,
  ...overrides,
});

const source = (config = {}) =>
  node("source", "source.json", {
    text: '{"stores":[{"id":"A-101","name":"Москва","state":1}]}',
    arrayPath: "/stores",
    ...config,
  });

test("execution plan binds every chain step to its direct parent output", () => {
  const document = graph(
    [
      source(),
      node("filter", "transform.filter", {
        mode: "all",
        conditions: [{ field: "state", operator: "equal", value: "1" }],
      }),
      node("project", "transform.project", { fields: ["id", "name"] }),
      node("sink", "sink.csv", { delimiter: ";" }),
    ],
    [
      connection("source-filter", "source", "records", "filter"),
      connection("filter-project", "filter", "matched", "project"),
      connection("project-sink", "project", "matched", "sink"),
    ],
  );

  const request = buildExecutionPlanRequest(document, { previewLimit: 25 });
  assert.equal(request.action, "execute_plan");
  assert.equal(request.plan.preview_limit, 25);
  assert.deepEqual(
    request.plan.steps.map(({ node_id, node_type, input }) => ({ node_id, node_type, input })),
    [
      { node_id: "source", node_type: "source", input: undefined },
      {
        node_id: "filter",
        node_type: "filter",
        input: { node_id: "source", port: "records" },
      },
      {
        node_id: "project",
        node_type: "project",
        input: { node_id: "filter", port: "matched" },
      },
      {
        node_id: "sink",
        node_type: "sink",
        input: { node_id: "project", port: "matched" },
      },
    ],
  );
});

test("execution plan preserves fan-out as independent direct-parent bindings", () => {
  const document = graph(
    [
      source(),
      node("left", "transform.project", { fields: ["id"] }),
      node("right", "transform.filter", {
        conditions: [{ field: "state", operator: "equal", value: "1" }],
      }),
    ],
    [
      connection("source-left", "source", "records", "left"),
      connection("source-right", "source", "records", "right"),
    ],
  );

  const steps = buildExecutionPlanRequest(document).plan.steps;
  assert.deepEqual(steps.map((step) => step.node_id), ["source", "left", "right"]);
  assert.deepEqual(steps[1].input, { node_id: "source", port: "records" });
  assert.deepEqual(steps[2].input, { node_id: "source", port: "records" });
});

test("template sink preserves the value template and line delimiter", () => {
  const document = graph(
    [
      node("source", "source.list", { text: '"A1",\n"B2"' }),
      node("sink", "sink.template", { template: "0x{value}", delimiter: ",\n" }),
    ],
    [connection("source-sink", "source", "values", "sink", "values")],
  );

  const sink = buildExecutionPlanRequest(document).plan.steps[1];
  assert.equal(sink.config.format, "template");
  assert.equal(sink.config.value_template, "0x{value}");
  assert.equal(sink.config.delimiter, ",\n");
  assert.equal(sink.config.strip_outer_quotes, true);
});

test("template transform keeps a value vector for a separate join step", () => {
  const document = graph(
    [
      node("source", "source.list", { text: '"A1",\n"B2"' }),
      node("template", "transform.template", { template: "0x{value}" }),
      node("join", "sink.join", { delimiter: ",\n" }),
    ],
    [
      connection("source-template", "source", "values", "template", "values"),
      connection("template-join", "template", "mapped", "join", "values"),
    ],
  );

  const request = buildExecutionPlanRequest(document);
  assert.deepEqual(request.plan.steps.map((step) => step.node_type), ["source", "template", "sink"]);
  assert.deepEqual(request.plan.steps[1].input, { node_id: "source", port: "values" });
  assert.equal(request.plan.steps[1].config.value_template, "0x{value}");
  assert.deepEqual(request.plan.steps[2].input, { node_id: "template", port: "mapped" });
  assert.equal(request.plan.steps[2].config.format, "flat");
  assert.equal(request.plan.steps[2].config.delimiter, ",\n");
});

test("plain list source compiles without JSON wrapping", () => {
  const document = graph(
    [
      node("source", "source.list", { text: '"AAA",\n"BBB"' }),
      node("sink", "sink.template", { template: "0x{value}", fields: ["value"] }),
    ],
    [connection("source-sink", "source", "values", "sink", "values")],
  );

  const request = buildExecutionPlanRequest(document);
  assert.equal(request.plan.steps[0].config.format, "list");
  assert.equal(request.plan.steps[0].config.path, "");
});

test("execution plan rejects an invalid graph instead of emitting a partial request", () => {
  const document = graph(
    [source(), node("project", "transform.project", { fields: ["id"] })],
    [],
  );

  assert.throws(
    () => buildExecutionPlanRequest(document),
    (error) => {
      assert.ok(error instanceof ExecutionPlanCompileError);
      assert.ok(error.issues.some((issue) => issue.code === "required_input_missing"));
      return true;
    },
  );
});

test("execution plan preserves filter then project semantic order", () => {
  const document = graph(
    [
      source(),
      node("filter", "transform.filter", {
        mode: "any",
        conditions: [{ field: "state", operator: "equal", value: "1" }],
      }),
      node("project", "transform.project", { fields: ["id", "name"] }),
    ],
    [
      connection("source-filter", "source", "records", "filter"),
      connection("filter-project", "filter", "matched", "project"),
    ],
  );

  const steps = buildExecutionPlanRequest(document).plan.steps;
  assert.deepEqual(steps.map((step) => step.node_type), ["source", "filter", "project"]);
  assert.deepEqual(steps[1].config.filters, [
    { field: "state", operator: "equal", value: "1" },
  ]);
  assert.deepEqual(steps[2].config.fields, ["id", "name"]);
  assert.deepEqual(steps[2].input, { node_id: "filter", port: "matched" });
});

test("execution plan preserves project then filter for runtime schema diagnosis", () => {
  const document = graph(
    [
      source(),
      node("project", "transform.project", { fields: ["id", "name"] }),
      node("filter", "transform.filter", {
        conditions: [{ field: "state", operator: "equal", value: "1" }],
      }),
    ],
    [
      connection("source-project", "source", "records", "project"),
      connection("project-filter", "project", "matched", "filter"),
    ],
  );

  const steps = buildExecutionPlanRequest(document).plan.steps;
  assert.deepEqual(steps.map((step) => step.node_type), ["source", "project", "filter"]);
  assert.deepEqual(steps[1].config.fields, ["id", "name"]);
  assert.deepEqual(steps[2].input, { node_id: "project", port: "matched" });
  assert.deepEqual(steps[2].config.filters, [
    { field: "state", operator: "equal", value: "1" },
  ]);
});
