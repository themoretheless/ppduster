import assert from "node:assert/strict";
import test from "node:test";

import { tokenizeJson } from "../src/ui/json-tokens.ts";

test("JSON tokenizer distinguishes keys and primitive values", () => {
  const tokens = tokenizeJson('{"name":"Москва","state":1,"active":true,"note":null}');
  const significant = tokens.filter((token) => token.kind !== "punctuation");
  assert.deepEqual(significant.map(({ kind, text }) => [kind, text]), [
    ["key", '"name"'],
    ["string", '"Москва"'],
    ["key", '"state"'],
    ["number", "1"],
    ["key", '"active"'],
    ["boolean", "true"],
    ["key", '"note"'],
    ["null", "null"],
  ]);
});

test("JSON tokenizer remains useful for incomplete editor input", () => {
  const tokens = tokenizeJson('{"name": "Москва');
  assert.equal(tokens.at(-1)?.kind, "invalid");
  assert.equal(tokens.map((token) => token.text).join(""), '{"name": "Москва');
});

test("JSON tokenizer handles escaped quotes and exponent numbers", () => {
  const tokens = tokenizeJson('{"text":"a\\\"b","value":-1.25e+3}');
  assert.ok(tokens.some((token) => token.kind === "string" && token.text === '"a\\\"b"'));
  assert.ok(tokens.some((token) => token.kind === "number" && token.text === "-1.25e+3"));
});
