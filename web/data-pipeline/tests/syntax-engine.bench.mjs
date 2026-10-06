import { performance } from "node:perf_hooks";

import { syntaxEngine } from "@peregon/syntax-engine";

const TARGET_SIZE = 1024 * 1024;
const ITERATIONS = 12;
const SOURCE_LINES = {
  csharp: "public sealed class Item { public string Name { get; init; } = \"value\"; }\n",
  rust: "pub struct Item<'a> { name: &'a str, enabled: bool } // value\n",
  javascript: "export const item = { name: \"value\", enabled: true, count: 42 };\n",
  typescript: "export interface Item { name: string; enabled: boolean; count: number }\n",
  python: "item = {\"name\": \"value\", \"enabled\": True, \"count\": 42}\n",
  sql: "SELECT name, enabled, count FROM items WHERE enabled = TRUE;\n",
  json: '{"name":"value","enabled":true,"count":42,"items":[1,2,3]}\n',
  xml: '<item name="value" enabled="true"><count>42</count></item>\n',
  yaml: "name: value\nenabled: true\ncount: 42\nitems: [1, 2, 3]\n",
  java: "public final class Item { String name = \"value\"; boolean enabled = true; }\n",
  ini: "[item]\nname = value\nenabled = true\ncount = 42\n",
};

function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))];
}

const results = [];
for (const [language, line] of Object.entries(SOURCE_LINES)) {
  const source = line.repeat(Math.ceil(TARGET_SIZE / line.length)).slice(0, TARGET_SIZE);
  const cold = [];
  for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
    const started = performance.now();
    syntaxEngine.createDocument(source, language);
    cold.push(performance.now() - started);
  }

  const document = syntaxEngine.createDocument(source, language);
  const local = [];
  const offset = Math.floor(source.length / 2);
  for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
    const replacement = iteration % 2 ? "x" : "y";
    const started = performance.now();
    document.applyChange({ from: offset, to: offset + 1, insert: replacement });
    local.push(performance.now() - started);
  }

  results.push({
    language,
    "cold p50 ms": percentile(cold, 0.5).toFixed(2),
    "cold p95 ms": percentile(cold, 0.95).toFixed(2),
    "edit p50 ms": percentile(local, 0.5).toFixed(2),
    "edit p95 ms": percentile(local, 0.95).toFixed(2),
  });
}

console.table(results);
