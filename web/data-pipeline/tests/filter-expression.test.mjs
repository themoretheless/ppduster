import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_FILTER_EXPRESSION_DEPTH,
  MAX_FILTER_EXPRESSION_NODES,
  isFilterExpression,
  legacyConditionsToExpression,
  normalizeFilterExpression,
  stableSerializeFilterExpression,
} from "../src/runtime/filter-expression.ts";
import { buildExecutionPlanRequest } from "../src/runtime/execute-plan.ts";

const condition = (field, operator = "equal", value = "1") => ({
  kind: "condition",
  field,
  operator,
  value,
});

test("nested AND/OR expression preserves group structure and child order", () => {
  const expression = {
    kind: "group",
    operator: "and",
    children: [
      condition("state"),
      {
        kind: "group",
        operator: "or",
        children: [
          condition("locality", "equal", "Москва"),
          condition("locality", "equal", "Белгород"),
        ],
      },
    ],
  };

  assert.equal(isFilterExpression(expression), true);
  assert.deepEqual(normalizeFilterExpression(expression), expression);
  assert.deepEqual(normalizeFilterExpression(expression).children[1].children.map((item) => item.value), [
    "Москва",
    "Белгород",
  ]);
});

test("NOT accepts a nested subtree rather than only a single flat condition", () => {
  const expression = {
    kind: "not",
    child: {
      kind: "group",
      operator: "or",
      children: [condition("state"), condition("name", "starts_with", "Тест")],
    },
  };

  assert.equal(isFilterExpression(expression), true);
  assert.deepEqual(normalizeFilterExpression(expression), expression);
});

test("conditions preserve JSON path quantifiers", () => {
  const expression = {
    kind: "condition",
    field: "tags[*]",
    quantifier: "any",
    operator: "equal",
    value: "vip",
  };

  assert.deepEqual(normalizeFilterExpression(expression), expression);
  assert.equal(isFilterExpression({ ...expression, quantifier: "sometimes" }), false);
});

test("legacy flat conditions migrate to the equivalent minimal expression", () => {
  const legacy = [
    { field: "state", operator: "equal", value: "1" },
    { field: "locality", operator: "equal", value: "Москва" },
  ];

  assert.deepEqual(legacyConditionsToExpression([], "all"), null);
  assert.deepEqual(legacyConditionsToExpression([legacy[0]], "all"), condition("state"));
  assert.deepEqual(legacyConditionsToExpression(legacy, "all"), {
    kind: "group",
    operator: "and",
    children: [condition("state"), condition("locality", "equal", "Москва")],
  });
  assert.deepEqual(legacyConditionsToExpression(legacy, "any"), {
    kind: "group",
    operator: "or",
    children: [condition("state"), condition("locality", "equal", "Москва")],
  });
});

test("normalization rejects malformed and empty expressions", () => {
  const malformed = [
    null,
    {},
    { kind: "condition", field: "state", operator: "unsupported", value: "1" },
    { kind: "condition", field: "", operator: "equal", value: "1" },
    { kind: "group", operator: "and", children: [] },
    { kind: "group", operator: "xor", children: [condition("state")] },
    { kind: "not" },
    { kind: "not", child: { kind: "group", operator: "and", children: [] } },
  ];

  for (const candidate of malformed) {
    assert.equal(normalizeFilterExpression(candidate), null);
    assert.equal(isFilterExpression(candidate), false);
  }
});

test("normalization guards against excessive expression depth and size", () => {
  let tooDeep = condition("state");
  for (let index = 0; index <= MAX_FILTER_EXPRESSION_DEPTH; index += 1) {
    tooDeep = { kind: "not", child: tooDeep };
  }
  assert.equal(normalizeFilterExpression(tooDeep), null);

  const tooLarge = {
    kind: "group",
    operator: "and",
    children: Array.from(
      { length: MAX_FILTER_EXPRESSION_NODES + 1 },
      (_, index) => condition(`field_${index}`),
    ),
  };
  assert.equal(normalizeFilterExpression(tooLarge), null);

  const cyclic = { kind: "not", child: null };
  cyclic.child = cyclic;
  assert.equal(normalizeFilterExpression(cyclic), null);
});

test("stable serialization canonicalizes object keys but preserves children order", () => {
  const first = {
    kind: "group",
    operator: "and",
    children: [condition("state"), condition("name", "contains", "склад")],
  };
  const sameWithDifferentKeyInsertion = {
    children: [
      { value: "1", operator: "equal", field: "state", kind: "condition" },
      { operator: "contains", value: "склад", kind: "condition", field: "name" },
    ],
    operator: "and",
    kind: "group",
  };
  const reversed = { ...first, children: [...first.children].reverse() };

  assert.equal(
    stableSerializeFilterExpression(first),
    stableSerializeFilterExpression(sameWithDifferentKeyInsertion),
  );
  assert.notEqual(
    stableSerializeFilterExpression(first),
    stableSerializeFilterExpression(reversed),
  );
  assert.equal(
    stableSerializeFilterExpression(first),
    '{"kind":"group","operator":"and","children":[{"kind":"condition","field":"state","operator":"equal","value":"1"},{"kind":"condition","field":"name","operator":"contains","value":"склад"}]}',
  );
});

test("execution-plan request serializes equivalent AST configs identically", () => {
  const expressionA = {
    kind: "group",
    operator: "and",
    children: [condition("state"), { kind: "not", child: condition("name", "contains", "архив") }],
  };
  const expressionB = {
    children: [
      { operator: "equal", value: "1", field: "state", kind: "condition" },
      {
        child: { value: "архив", kind: "condition", field: "name", operator: "contains" },
        kind: "not",
      },
    ],
    operator: "and",
    kind: "group",
  };
  const makeGraph = (expression) => ({
    version: 2,
    id: "stable-filter-request",
    revision: 4,
    nodes: [
      {
        id: "source",
        type: "source.json",
        position: { x: 0, y: 0 },
        config: { text: "[]" },
      },
      {
        id: "filter",
        type: "transform.filter",
        position: { x: 100, y: 0 },
        config: { expression },
      },
    ],
    connections: [
      {
        id: "source-filter",
        from: { nodeId: "source", port: "records" },
        to: { nodeId: "filter", port: "records" },
      },
    ],
  });

  const first = buildExecutionPlanRequest(makeGraph(expressionA));
  const second = buildExecutionPlanRequest(makeGraph(expressionB));
  assert.equal(JSON.stringify(first), JSON.stringify(second));
  assert.deepEqual(first.plan.steps[1].config.expression, expressionA);
});
