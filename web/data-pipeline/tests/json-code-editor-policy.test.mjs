import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_HIGHLIGHT_SOURCE_LENGTH,
  MAX_HIGHLIGHT_TOKENS,
  canBuildSyntaxSnapshot,
  dominantEol,
  exceedsHighlightTokenLimit,
  withEditorEol,
} from "../src/components/json-code-editor-policy.ts";
import { computeTextChange } from "@peregon/syntax-engine";

test("editor preserves the dominant model EOL across textarea LF normalization", () => {
  assert.equal(dominantEol("a\r\nb\r\nc\nd"), "\r\n");
  assert.equal(dominantEol("a\nb\r\nc"), "\n");
  assert.equal(dominantEol("single line", "\r\n"), "\r\n");

  const before = "a\r\nb\r\nc😀";
  const textareaAfterTyping = "a\nb\nc😀!";
  const emitted = withEditorEol(textareaAfterTyping, dominantEol(before));
  assert.equal(emitted, "a\r\nb\r\nc😀!");
  assert.deepEqual(computeTextChange(before, emitted), {
    from: before.length,
    to: before.length,
    insert: "!",
  });
});

test("editor normalizes mixed textarea newlines to an explicit convention", () => {
  assert.equal(withEditorEol("a\r\nb\rc\nd", "\n"), "a\nb\nc\nd");
  assert.equal(withEditorEol("a\r\nb\rc\nd", "\r"), "a\rb\rc\rd");
  assert.equal(withEditorEol("ключ😀\nvalue", "\r\n"), "ключ😀\r\nvalue");
});

test("large or token-dense editor values bypass span-based highlighting", () => {
  assert.equal(canBuildSyntaxSnapshot("x".repeat(MAX_HIGHLIGHT_SOURCE_LENGTH), true), true);
  assert.equal(canBuildSyntaxSnapshot("x".repeat(MAX_HIGHLIGHT_SOURCE_LENGTH + 1), true), false);
  assert.equal(canBuildSyntaxSnapshot("small", false), false);

  const atLimit = [{ tokens: new Array(MAX_HIGHLIGHT_TOKENS) }];
  const overLimit = [{ tokens: new Array(MAX_HIGHLIGHT_TOKENS + 1) }];
  assert.equal(exceedsHighlightTokenLimit(atLimit), false);
  assert.equal(exceedsHighlightTokenLimit(overLimit), true);

  const largeValue = `[${"0,".repeat(512 * 1024)}0]`;
  assert.equal(canBuildSyntaxSnapshot(largeValue, true), false);
  assert.equal(withEditorEol(largeValue, "\n"), largeValue);
});
