import assert from "node:assert/strict";
import test from "node:test";

import { ValueVectorParseError, parseValueVector } from "../src/engine/value-vector-parser.ts";

test("plain lists become positional scalar tokens without JSON records", () => {
  const source = '"AAA",\n\'BBB\',\nCCC';
  assert.deepEqual(parseValueVector(source), [
    { position: 0, value: "AAA", sourceStart: 0, sourceEnd: 5, quoted: true },
    { position: 1, value: "BBB", sourceStart: 7, sourceEnd: 12, quoted: true },
    { position: 2, value: "CCC", sourceStart: 14, sourceEnd: 17, quoted: false },
  ]);
});

test("commas inside quotes remain part of the scalar value", () => {
  assert.deepEqual(
    parseValueVector('"A,B", C').map(({ value }) => value),
    ["A,B", "C"],
  );
});

test("unterminated quotes report their source offset", () => {
  assert.throws(
    () => parseValueVector('"AAA'),
    (error) => error instanceof ValueVectorParseError && error.offset === 4,
  );
});
