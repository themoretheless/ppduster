import assert from "node:assert/strict";
import test from "node:test";

import {
  buildExecutePlanSchemaState,
  diagnoseStaleNodeFields,
  inputFieldsForNode,
  outputFieldsForNode,
  staleFieldNames,
} from "../src/runtime/schema-state.ts";

const field = (name, kind = "string", present = 2) => ({ name, kind, present });
const schema = (...fields) => ({ kind: "records", fields });

const stats = (input_items, output_items) => ({
  input_items,
  output_items,
  filtered_out: input_items - output_items,
  skipped_items: 0,
});

const nodeResult = ({ inputFields, outputFields, preview = [], inputItems = 2 }) => ({
  ok: true,
  preview,
  preview_truncated: false,
  input_schema: inputFields === undefined || inputFields === null ? null : schema(...inputFields),
  output_schema: outputFields === null ? null : schema(...outputFields),
  // Compatibility alias must not be the only schema source used by new UI code.
  schema: outputFields === null ? null : schema(...outputFields),
  stats: stats(inputItems, preview.length),
  diagnostics: [],
});

const sourceFields = [
  field("id"),
  field("name"),
  field("locality"),
  field("state", "number"),
];
const projectedFields = [field("id"), field("name")];

const request = {
  action: "execute_plan",
  plan: {
    version: 1,
    graph_id: "schema-contract",
    graph_revision: 1,
    preview_limit: 20,
    steps: [
      {
        node_id: "source",
        node_type: "source",
        config: { data: "[]", format: "json", path: "", csv_delimiter: "," },
      },
      {
        node_id: "project",
        node_type: "project",
        input: { node_id: "source", port: "records" },
        config: { fields: ["id", "name"] },
      },
      {
        node_id: "downstreamFilter",
        node_type: "filter",
        input: { node_id: "project", port: "matched" },
        config: {
          filters: [
            { field: "state", operator: "equal", value: "1" },
            { field: "locality", operator: "equal", value: "Москва" },
          ],
          filter_mode: "all",
        },
      },
      {
        node_id: "emptyFilter",
        node_type: "filter",
        input: { node_id: "project", port: "matched" },
        config: { filters: [], filter_mode: "all" },
      },
    ],
  },
};

const rawResponse = {
  ok: true,
  nodes: {
    source: nodeResult({
      outputFields: sourceFields,
      preview: [{ id: "A-101", name: "Москва", locality: "Москва", state: 1 }],
    }),
    project: nodeResult({
      inputFields: sourceFields,
      outputFields: projectedFields,
      preview: [{ id: "A-101", name: "Москва" }],
    }),
    downstreamFilter: nodeResult({
      inputFields: projectedFields,
      outputFields: projectedFields,
      preview: [{ id: "A-101", name: "Москва" }],
    }),
    emptyFilter: nodeResult({
      inputFields: projectedFields,
      outputFields: projectedFields,
      preview: [],
    }),
  },
  sink_outputs: {},
  diagnostics: [],
};

const response = buildExecutePlanSchemaState(request, rawResponse);

test("downstream field options come only from that node input_schema", () => {
  const fields = inputFieldsForNode(response, "downstreamFilter");

  assert.deepEqual(fields, projectedFields);
  assert.deepEqual(fields.map((candidate) => candidate.name), ["id", "name"]);
  assert.equal(fields.some((candidate) => candidate.name === "state"), false);
  assert.equal(fields.some((candidate) => candidate.name === "locality"), false);
});

test("project output schema physically excludes fields removed by projection", () => {
  assert.deepEqual(outputFieldsForNode(response, "source"), sourceFields);
  assert.deepEqual(outputFieldsForNode(response, "project"), projectedFields);
  assert.deepEqual(
    outputFieldsForNode(response, "project").map((candidate) => candidate.name),
    ["id", "name"],
  );
});

test("an empty result preserves input and output schemas for downstream editing", () => {
  assert.deepEqual(rawResponse.nodes.emptyFilter.preview, []);
  assert.deepEqual(inputFieldsForNode(response, "emptyFilter"), projectedFields);
  assert.deepEqual(outputFieldsForNode(response, "emptyFilter"), projectedFields);
});

test("stale selections are diagnosed against the node input_schema", () => {
  assert.deepEqual(
    staleFieldNames(response, "downstreamFilter", ["id", "state", "locality"]),
    ["state", "locality"],
  );
  assert.deepEqual(staleFieldNames(response, "downstreamFilter", ["id", "name"]), []);
  assert.deepEqual(
    response.nodes.downstreamFilter.diagnostics.map(({ code, field }) => ({ code, field })),
    [
      { code: "condition_field_not_in_input_schema", field: "state" },
      { code: "condition_field_not_in_input_schema", field: "locality" },
    ],
  );
});

test("nested and root JSON paths validate against their top-level input field", () => {
  const step = {
    node_id: "pathFilter",
    node_type: "filter",
    input: { node_id: "source", port: "records" },
    config: {
      filters: [],
      filter_mode: "all",
      expression: {
        kind: "group",
        operator: "and",
        children: [
          { kind: "condition", field: "profile.age", operator: "exists" },
          { kind: "condition", field: "$.tags[*]", quantifier: "any", operator: "exists" },
          { kind: "condition", field: "$", operator: "exists" },
        ],
      },
    },
  };

  assert.deepEqual(
    diagnoseStaleNodeFields(step, schema(field("profile", "object"), field("tags", "array"))),
    [],
  );
});

test("missing nodes and source nodes without input_schema expose no input fields", () => {
  assert.deepEqual(inputFieldsForNode(response, "source"), []);
  assert.deepEqual(inputFieldsForNode(response, "missing"), []);
  assert.deepEqual(outputFieldsForNode(response, "missing"), []);
  // With no schema there is no evidence that a saved field is stale yet.
  assert.deepEqual(staleFieldNames(response, "missing", ["id"]), []);

  // A known empty schema is different: it proves that no selected field exists.
  const knownEmptyInput = {
    nodes: { empty: { input_schema: schema(), output_schema: schema() } },
    diagnostics: [],
  };
  assert.deepEqual(staleFieldNames(knownEmptyInput, "empty", ["id"]), ["id"]);
});
