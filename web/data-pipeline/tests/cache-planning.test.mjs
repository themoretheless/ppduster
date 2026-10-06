import assert from "node:assert/strict";
import test from "node:test";

import {
  EXECUTE_PLAN_CACHE_VERSION,
  canonicalizeForCache,
  computeExecutePlanStepCacheKey,
  stableSerializeForCache,
} from "../src/runtime/cache-key.ts";
import { buildExecutionPlanRequest } from "../src/runtime/execute-plan.ts";

const condition = (field, value) => ({
  kind: "condition",
  field,
  operator: "equal",
  value,
});

const baseGraph = () => ({
  version: 2,
  id: "cache-contract",
  revision: 1,
  nodes: [
    {
      id: "source",
      type: "source.json",
      position: { x: 0, y: 0 },
      config: {
        text: '{"stores":[{"id":"A","name":"Москва","locality":"Москва","state":1}]}',
        arrayPath: "/stores",
      },
    },
    {
      id: "middle",
      type: "transform.filter",
      position: { x: 300, y: -100 },
      config: {
        expression: {
          kind: "group",
          operator: "and",
          children: [condition("state", "1"), condition("locality", "Москва")],
        },
      },
    },
    {
      id: "downstream",
      type: "transform.project",
      position: { x: 600, y: -100 },
      config: { fields: ["id", "name"] },
    },
    {
      id: "sibling",
      type: "transform.project",
      position: { x: 300, y: 180 },
      config: { fields: ["id"] },
    },
  ],
  connections: [
    {
      id: "source-middle",
      from: { nodeId: "source", port: "records" },
      to: { nodeId: "middle", port: "records" },
    },
    {
      id: "middle-downstream",
      from: { nodeId: "middle", port: "matched" },
      to: { nodeId: "downstream", port: "records" },
    },
    {
      id: "source-sibling",
      from: { nodeId: "source", port: "records" },
      to: { nodeId: "sibling", port: "records" },
    },
  ],
});

const cacheKeys = (graph) => Object.fromEntries(
  buildExecutionPlanRequest(graph).plan.steps.map((step) => [step.node_id, step.cache_key]),
);

test("identical executable plans produce stable compact cache keys", () => {
  const first = cacheKeys(baseGraph());
  const second = cacheKeys(structuredClone(baseGraph()));

  assert.deepEqual(first, second);
  for (const key of Object.values(first)) {
    assert.match(key, new RegExp(`^${EXECUTE_PLAN_CACHE_VERSION}-[0-9a-f]{16}$`));
  }
});

test("titles, canvas positions, and graph revision do not invalidate execution keys", () => {
  const original = baseGraph();
  const visualEdit = structuredClone(original);
  visualEdit.revision += 1;
  visualEdit.nodes.forEach((node, index) => {
    node.position = { x: node.position.x + 900 + index, y: node.position.y - 400 };
    node.config.title = `Переименованный блок ${index + 1}`;
  });

  assert.deepEqual(cacheKeys(visualEdit), cacheKeys(original));
});

test("source data changes invalidate the source and every descendant branch", () => {
  const original = cacheKeys(baseGraph());
  const changed = baseGraph();
  changed.nodes.find((node) => node.id === "source").config.text =
    '{"stores":[{"id":"B","name":"Белгород","locality":"Белгород","state":0}]}';
  const next = cacheKeys(changed);

  assert.notEqual(next.source, original.source);
  assert.notEqual(next.middle, original.middle);
  assert.notEqual(next.downstream, original.downstream);
  assert.notEqual(next.sibling, original.sibling);
});

test("middle condition changes invalidate only that node and its descendants", () => {
  const original = cacheKeys(baseGraph());
  const changed = baseGraph();
  changed.nodes.find((node) => node.id === "middle")
    .config.expression.children[0].value = "0";
  const next = cacheKeys(changed);

  assert.equal(next.source, original.source);
  assert.notEqual(next.middle, original.middle);
  assert.notEqual(next.downstream, original.downstream);
  assert.equal(next.sibling, original.sibling);
});

test("AST child order affects its branch cache key", () => {
  const original = cacheKeys(baseGraph());
  const changed = baseGraph();
  const middle = changed.nodes.find((node) => node.id === "middle");
  middle.config.expression.children.reverse();
  const next = cacheKeys(changed);

  assert.equal(next.source, original.source);
  assert.notEqual(next.middle, original.middle);
  assert.notEqual(next.downstream, original.downstream);
  assert.equal(next.sibling, original.sibling);
});

test("runtime and visual state never participates in direct step key computation", () => {
  const clean = {
    node_type: "project",
    config: { fields: ["id", "name"] },
    input: { node_id: "source", port: "records" },
  };
  const polluted = {
    ...clean,
    title: "Новый заголовок",
    position: { x: 100, y: 200 },
    preview: [{ id: "secret" }],
    stats: { duration_ms: 999 },
    runtime: { dataset: "temporary" },
    output: "temporary output",
    cache: { hit: true },
    cache_key: "old-key",
  };

  assert.deepEqual(canonicalizeForCache(polluted), canonicalizeForCache(clean));
  assert.equal(stableSerializeForCache(polluted), stableSerializeForCache(clean));
  assert.equal(
    computeExecutePlanStepCacheKey(polluted, "parent-key"),
    computeExecutePlanStepCacheKey(clean, "parent-key"),
  );
});
