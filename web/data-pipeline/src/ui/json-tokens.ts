export type JsonTokenKind =
  | "key"
  | "string"
  | "number"
  | "boolean"
  | "null"
  | "punctuation"
  | "whitespace"
  | "invalid";

export interface JsonToken {
  readonly kind: JsonTokenKind;
  readonly text: string;
}

import { syntaxEngine, type TokenKind } from "@peregon/syntax-engine";

const JSON_KIND: Partial<Record<TokenKind, JsonTokenKind>> = {
  property: "key",
  string: "string",
  number: "number",
  boolean: "boolean",
  null: "null",
  punctuation: "punctuation",
  operator: "punctuation",
  whitespace: "whitespace",
  invalid: "invalid",
  text: "invalid",
};

/**
 * Compatibility adapter for the original JSON-only UI API. New code should use
 * SyntaxDocument directly so unchanged lines retain their token snapshots.
 */
export function tokenizeJson(source: string): JsonToken[] {
  const document = syntaxEngine.createDocument(source, "json");
  const diagnostics = document.lexicalDiagnostics().filter((item) => item.severity === "error");
  return document.tokens().map((token) => ({
    kind: diagnostics.some((item) => item.from < token.to && item.to > token.from)
      ? "invalid"
      : JSON_KIND[token.kind] ?? "invalid",
    text: source.slice(token.from, token.to),
  }));
}
