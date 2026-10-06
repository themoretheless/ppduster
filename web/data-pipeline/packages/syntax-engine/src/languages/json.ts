import type {
  FormatOptions,
  FormatResult,
  LanguageDefinition,
  LineDiagnostic,
  LineToken,
  SyntaxDiagnostic,
  TokenKind,
} from "../types.js";

type ObjectExpectation = "key-or-end" | "key" | "colon" | "value" | "comma-or-end";
type ArrayExpectation = "value-or-end" | "value" | "comma-or-end";

type JsonFrame =
  | { readonly kind: "object"; readonly expect: ObjectExpectation }
  | { readonly kind: "array"; readonly expect: ArrayExpectation };

interface JsonStackNode {
  readonly kind: JsonFrame["kind"];
  readonly expect: JsonFrame["expect"];
  readonly parent: JsonStackNode | null;
  readonly depth: number;
  readonly hash: number;
}

export interface JsonLexerState {
  readonly root: "value" | "end";
  readonly stack: JsonStackNode | null;
  readonly overflowDepth: number;
}

const MAX_LEXICAL_DEPTH = 2048;
const JSON_NUMBER = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/;
const JSON_WHITESPACE = /^[\u0020\t\r\n]+/;
const DELIMITER = /[\u0020\t\r\n,\]}]/u;

function initialState(): JsonLexerState {
  return Object.freeze({ root: "value", stack: null, overflowDepth: 0 });
}

function frameHash(parent: JsonStackNode | null, frame: JsonFrame): number {
  const kind = frame.kind === "object" ? 11 : 17;
  const expect = frame.expect === "key-or-end"
    ? 23
    : frame.expect === "key"
      ? 29
      : frame.expect === "colon"
        ? 31
        : frame.expect === "value"
          ? 37
          : frame.expect === "value-or-end"
            ? 41
            : 43;
  return (Math.imul(parent?.hash ?? 2166136261, 16777619) ^ kind ^ expect) >>> 0;
}

function node(frame: JsonFrame, parent: JsonStackNode | null): JsonStackNode {
  return Object.freeze({
    ...frame,
    parent,
    depth: (parent?.depth ?? 0) + 1,
    hash: frameHash(parent, frame),
  });
}

function replaceTop(state: JsonLexerState, frame: JsonFrame): JsonLexerState {
  return Object.freeze({ ...state, stack: node(frame, state.stack?.parent ?? null) });
}

function pushFrame(state: JsonLexerState, frame: JsonFrame): JsonLexerState {
  return Object.freeze({ ...state, stack: node(frame, state.stack) });
}

function consumeValue(state: JsonLexerState): JsonLexerState {
  const top = state.stack;
  if (!top) return Object.freeze({ ...state, root: "end" });
  if (top.kind === "object" && top.expect === "value") {
    return replaceTop(state, { kind: "object", expect: "comma-or-end" });
  }
  if (top.kind === "array" && (top.expect === "value-or-end" || top.expect === "value")) {
    return replaceTop(state, { kind: "array", expect: "comma-or-end" });
  }
  return state;
}

function expectsValue(state: JsonLexerState): boolean {
  const top = state.stack;
  if (!top) return state.root === "value";
  return (top.kind === "object" && top.expect === "value")
    || (top.kind === "array" && (top.expect === "value-or-end" || top.expect === "value"));
}

function statesEqual(left: Readonly<JsonLexerState>, right: Readonly<JsonLexerState>): boolean {
  if (left === right) return true;
  if (left.root !== right.root || left.overflowDepth !== right.overflowDepth) return false;
  let leftNode = left.stack;
  let rightNode = right.stack;
  if (leftNode === rightNode) return true;
  if (
    leftNode?.depth !== rightNode?.depth
    || leftNode?.hash !== rightNode?.hash
  ) return false;
  while (leftNode && rightNode) {
    if (leftNode === rightNode) return true;
    if (leftNode.kind !== rightNode.kind || leftNode.expect !== rightNode.expect) return false;
    leftNode = leftNode.parent;
    rightNode = rightNode.parent;
  }
  return leftNode === rightNode;
}

function stringEnd(source: string, from: number): {
  readonly end: number;
  readonly diagnostics: readonly LineDiagnostic[];
} {
  const diagnostics: LineDiagnostic[] = [];
  let index = from + 1;
  while (index < source.length) {
    const code = source.charCodeAt(index);
    if (code === 34) return { end: index + 1, diagnostics };
    if (code < 0x20) {
      diagnostics.push({
        from: index,
        to: index + 1,
        severity: "error",
        code: "json-control-character",
        message: "Управляющий символ в JSON-строке должен быть экранирован",
      });
      index += 1;
      continue;
    }
    if (code !== 92) {
      index += 1;
      continue;
    }
    const escape = source[index + 1];
    if (!escape) break;
    if ('"\\/bfnrt'.includes(escape)) {
      index += 2;
      continue;
    }
    if (escape === "u") {
      let end = index + 2;
      const expectedEnd = Math.min(source.length, index + 6);
      while (end < expectedEnd && /[\da-fA-F]/u.test(source[end])) end += 1;
      if (end === index + 6) {
        index = end;
        continue;
      }
      diagnostics.push({
        from: index,
        to: Math.max(index + 2, end),
        severity: "error",
        code: "json-invalid-escape",
        message: "Недопустимая escape-последовательность JSON",
      });
      // Resume before a quote/brace instead of swallowing structure while
      // trying to consume the four missing hexadecimal digits.
      index = Math.max(index + 2, end);
      continue;
    }
    diagnostics.push({
      from: index,
      to: Math.min(source.length, index + 2),
      severity: "error",
      code: "json-invalid-escape",
      message: "Недопустимая escape-последовательность JSON",
    });
    index += 2;
  }
  diagnostics.push({
    from,
    to: source.length,
    severity: "error",
    code: "json-unterminated-string",
    message: "Незакрытая JSON-строка",
  });
  return { end: source.length, diagnostics };
}

function invalidEnd(line: string, from: number): number {
  let index = from + 1;
  while (index < line.length && !/[\u0020\t,\[\]{}:"]/u.test(line[index])) index += 1;
  return index;
}

function diagnostic(from: number, to: number, message: string, code: string): LineDiagnostic {
  return { from, to, message, code, severity: "error" };
}

function tokenizeJsonLine(line: string, startState: Readonly<JsonLexerState>) {
  const tokens: LineToken[] = [];
  const diagnostics: LineDiagnostic[] = [];
  let state = startState as JsonLexerState;
  let index = 0;

  const push = (from: number, to: number, kind: TokenKind) => tokens.push({ from, to, kind });
  const unexpected = (from: number, to: number, expected: string) => diagnostics.push(
    diagnostic(from, to, `Ожидалось: ${expected}`, "json-unexpected-token"),
  );

  while (index < line.length) {
    if (/[\u0020\t]/u.test(line[index])) {
      const from = index++;
      while (index < line.length && /[\u0020\t]/u.test(line[index])) index += 1;
      push(from, index, "whitespace");
      continue;
    }

    const character = line[index];
    const top = state.stack;
    if (character === '"') {
      const parsed = stringEnd(line, index);
      const isProperty = state.overflowDepth === 0
        && top?.kind === "object"
        && (top.expect === "key-or-end" || top.expect === "key");
      push(index, parsed.end, isProperty ? "property" : "string");
      diagnostics.push(...parsed.diagnostics);
      if (!parsed.diagnostics.some((item) => item.code === "json-unterminated-string")) {
        if (state.overflowDepth > 0) {
          // Structure below the safety limit remains intact while overflow is skipped.
        } else if (isProperty) state = replaceTop(state, { kind: "object", expect: "colon" });
        else if (expectsValue(state)) state = consumeValue(state);
        else unexpected(index, parsed.end, top?.kind === "object" ? "ключ объекта" : "разделитель");
      }
      index = parsed.end;
      continue;
    }

    if (character === "{") {
      push(index, index + 1, "punctuation");
      if (state.overflowDepth > 0) state = Object.freeze({ ...state, overflowDepth: state.overflowDepth + 1 });
      else if (!expectsValue(state)) unexpected(index, index + 1, "значение");
      else if ((state.stack?.depth ?? 0) >= MAX_LEXICAL_DEPTH) {
        diagnostics.push(diagnostic(index, index + 1, "Превышена допустимая глубина JSON", "json-depth-limit"));
        state = Object.freeze({ ...state, overflowDepth: 1 });
      } else state = pushFrame(state, { kind: "object", expect: "key-or-end" });
      index += 1;
      continue;
    }
    if (character === "[") {
      push(index, index + 1, "punctuation");
      if (state.overflowDepth > 0) state = Object.freeze({ ...state, overflowDepth: state.overflowDepth + 1 });
      else if (!expectsValue(state)) unexpected(index, index + 1, "значение");
      else if ((state.stack?.depth ?? 0) >= MAX_LEXICAL_DEPTH) {
        diagnostics.push(diagnostic(index, index + 1, "Превышена допустимая глубина JSON", "json-depth-limit"));
        state = Object.freeze({ ...state, overflowDepth: 1 });
      } else state = pushFrame(state, { kind: "array", expect: "value-or-end" });
      index += 1;
      continue;
    }
    if (character === "}" || character === "]") {
      push(index, index + 1, "punctuation");
      if (state.overflowDepth > 0) {
        const overflowDepth = state.overflowDepth - 1;
        state = Object.freeze({ ...state, overflowDepth });
        if (overflowDepth === 0) state = consumeValue(state);
        index += 1;
        continue;
      }
      const matches = (character === "}" && top?.kind === "object")
        || (character === "]" && top?.kind === "array");
      const canClose = top?.expect === "key-or-end"
        || top?.expect === "value-or-end"
        || top?.expect === "comma-or-end";
      if (!matches) unexpected(index, index + 1, character === "}" ? "член объекта" : "элемент массива");
      else if (!canClose) {
        diagnostics.push(diagnostic(
          index,
          index + 1,
          top?.expect === "key" || (top?.kind === "array" && top.expect === "value")
            ? "Запятая в конце JSON-контейнера недопустима"
            : `Ожидалось: ${character === "}" ? "член объекта" : "элемент массива"}`,
          top?.expect === "key" || (top?.kind === "array" && top.expect === "value")
            ? "json-trailing-comma"
            : "json-unexpected-token",
        ));
        state = Object.freeze({ ...state, stack: state.stack?.parent ?? null });
        state = consumeValue(state);
      }
      else {
        state = Object.freeze({ ...state, stack: state.stack?.parent ?? null });
        state = consumeValue(state);
      }
      index += 1;
      continue;
    }
    if (character === ":") {
      push(index, index + 1, "punctuation");
      if (state.overflowDepth > 0) {
        // Ignore structure beyond the configured safety depth.
      } else if (top?.kind === "object" && top.expect === "colon") {
        state = replaceTop(state, { kind: "object", expect: "value" });
      } else unexpected(index, index + 1, "ключ объекта");
      index += 1;
      continue;
    }
    if (character === ",") {
      push(index, index + 1, "punctuation");
      if (state.overflowDepth > 0) {
        // Ignore structure beyond the configured safety depth.
      } else if (top?.kind === "object" && top.expect === "comma-or-end") {
        state = replaceTop(state, { kind: "object", expect: "key" });
      } else if (top?.kind === "array" && top.expect === "comma-or-end") {
        state = replaceTop(state, { kind: "array", expect: "value" });
      } else unexpected(index, index + 1, "значение");
      index += 1;
      continue;
    }

    const number = line.slice(index).match(JSON_NUMBER)?.[0] ?? "";
    if (number && (index + number.length === line.length || DELIMITER.test(line[index + number.length]))) {
      push(index, index + number.length, "number");
      if (state.overflowDepth > 0) {
        // Ignore structure beyond the configured safety depth.
      } else if (expectsValue(state)) state = consumeValue(state);
      else unexpected(index, index + number.length, "разделитель");
      index += number.length;
      continue;
    }

    const literal = [
      ["true", "boolean"],
      ["false", "boolean"],
      ["null", "null"],
    ] as const;
    const matched = literal.find(([value]) => line.startsWith(value, index)
      && (index + value.length === line.length || DELIMITER.test(line[index + value.length])));
    if (matched) {
      push(index, index + matched[0].length, matched[1]);
      if (state.overflowDepth > 0) {
        // Ignore structure beyond the configured safety depth.
      } else if (expectsValue(state)) state = consumeValue(state);
      else unexpected(index, index + matched[0].length, "разделитель");
      index += matched[0].length;
      continue;
    }

    const end = invalidEnd(line, index);
    push(index, end, "invalid");
    diagnostics.push(diagnostic(index, end, "Недопустимый токен JSON", "json-invalid-token"));
    index = end;
  }

  return { tokens, diagnostics, state };
}

interface RawNode {
  readonly kind: "raw";
  readonly raw: string;
}

interface ArrayNode {
  readonly kind: "array";
  readonly items: readonly JsonNode[];
}

interface ObjectNode {
  readonly kind: "object";
  readonly members: readonly { readonly key: string; readonly value: JsonNode }[];
}

type JsonNode = RawNode | ArrayNode | ObjectNode;

class JsonReader {
  index = 0;
  diagnostic: SyntaxDiagnostic | null = null;
  readonly source: string;

  constructor(source: string) {
    this.source = source;
  }

  parse(): JsonNode | null {
    this.skipWhitespace();
    const node = this.value(0);
    if (!node) return null;
    this.skipWhitespace();
    if (this.index !== this.source.length) {
      this.fail(this.index, Math.min(this.source.length, this.index + 1), "После корневого значения есть лишние данные", "json-trailing-data");
      return null;
    }
    return node;
  }

  private value(depth: number): JsonNode | null {
    if (depth > 512) return this.fail(this.index, this.index, "Слишком глубокая вложенность JSON", "json-depth-limit");
    this.skipWhitespace();
    const from = this.index;
    const character = this.source[this.index];
    if (character === "{") return this.object(depth + 1);
    if (character === "[") return this.array(depth + 1);
    if (character === '"') {
      const raw = this.string();
      return raw === null ? null : { kind: "raw", raw };
    }
    for (const literal of ["true", "false", "null"]) {
      if (this.source.startsWith(literal, this.index)) {
        this.index += literal.length;
        if (!this.isDelimiter(this.source[this.index])) {
          return this.fail(from, this.scanInvalid(from), "Недопустимый литерал JSON", "json-invalid-literal");
        }
        return { kind: "raw", raw: literal };
      }
    }
    const number = this.source.slice(this.index).match(JSON_NUMBER)?.[0] ?? "";
    if (number) {
      this.index += number.length;
      if (!this.isDelimiter(this.source[this.index])) {
        return this.fail(from, this.scanInvalid(from), "Некорректное число JSON", "json-invalid-number");
      }
      return { kind: "raw", raw: number };
    }
    return this.fail(from, Math.min(this.source.length, from + 1), "Ожидалось значение JSON", "json-expected-value");
  }

  private object(depth: number): ObjectNode | null {
    this.index += 1;
    this.skipWhitespace();
    const members: Array<{ key: string; value: JsonNode }> = [];
    if (this.take("}")) return { kind: "object", members };
    while (this.index < this.source.length) {
      const keyFrom = this.index;
      if (this.source[this.index] !== '"') {
        return this.fail(keyFrom, Math.min(this.source.length, keyFrom + 1), "Ключ объекта должен быть строкой", "json-expected-key");
      }
      const key = this.string();
      if (key === null) return null;
      this.skipWhitespace();
      if (!this.take(":")) return this.fail(this.index, this.index, "После ключа требуется двоеточие", "json-expected-colon");
      const value = this.value(depth);
      if (!value) return null;
      members.push({ key, value });
      this.skipWhitespace();
      if (this.take("}")) return { kind: "object", members };
      if (!this.take(",")) return this.fail(this.index, this.index, "Между членами объекта требуется запятая", "json-expected-comma");
      this.skipWhitespace();
      if (this.source[this.index] === "}") {
        return this.fail(this.index, this.index + 1, "В JSON нельзя оставлять запятую перед }", "json-trailing-comma");
      }
    }
    return this.fail(this.index, this.index, "Объект JSON не закрыт", "json-unclosed-object");
  }

  private array(depth: number): ArrayNode | null {
    this.index += 1;
    this.skipWhitespace();
    const items: JsonNode[] = [];
    if (this.take("]")) return { kind: "array", items };
    while (this.index < this.source.length) {
      const value = this.value(depth);
      if (!value) return null;
      items.push(value);
      this.skipWhitespace();
      if (this.take("]")) return { kind: "array", items };
      if (!this.take(",")) return this.fail(this.index, this.index, "Между элементами массива требуется запятая", "json-expected-comma");
      this.skipWhitespace();
      if (this.source[this.index] === "]") {
        return this.fail(this.index, this.index + 1, "В JSON нельзя оставлять запятую перед ]", "json-trailing-comma");
      }
    }
    return this.fail(this.index, this.index, "Массив JSON не закрыт", "json-unclosed-array");
  }

  private string(): string | null {
    const from = this.index++;
    while (this.index < this.source.length) {
      const code = this.source.charCodeAt(this.index);
      if (code === 34) {
        this.index += 1;
        return this.source.slice(from, this.index);
      }
      if (code < 0x20) {
        return this.fail(this.index, this.index + 1, "Управляющий символ в строке должен быть экранирован", "json-control-character");
      }
      if (code !== 92) {
        this.index += 1;
        continue;
      }
      const escape = this.source[this.index + 1];
      if ('"\\/bfnrt'.includes(escape ?? "")) {
        this.index += 2;
        continue;
      }
      if (escape === "u" && /^[\da-fA-F]{4}$/u.test(this.source.slice(this.index + 2, this.index + 6))) {
        this.index += 6;
        continue;
      }
      return this.fail(this.index, Math.min(this.source.length, this.index + 6), "Недопустимая escape-последовательность JSON", "json-invalid-escape");
    }
    return this.fail(from, this.source.length, "Незакрытая JSON-строка", "json-unterminated-string");
  }

  private skipWhitespace(): void {
    const whitespace = this.source.slice(this.index).match(JSON_WHITESPACE)?.[0];
    if (whitespace) this.index += whitespace.length;
  }

  private take(value: string): boolean {
    if (!this.source.startsWith(value, this.index)) return false;
    this.index += value.length;
    return true;
  }

  private isDelimiter(character: string | undefined): boolean {
    return character === undefined || DELIMITER.test(character);
  }

  private scanInvalid(from: number): number {
    let end = Math.max(this.index, from + 1);
    while (end < this.source.length && !this.isDelimiter(this.source[end])) end += 1;
    this.index = end;
    return end;
  }

  private fail<T extends null>(from: number, to: number, message: string, code: string): T {
    if (!this.diagnostic) {
      this.diagnostic = { from, to: Math.max(from, to), message, code, severity: "error" };
    }
    return null as T;
  }
}

function parseJson(source: string): { node: JsonNode | null; diagnostics: readonly SyntaxDiagnostic[] } {
  const reader = new JsonReader(source);
  const node = reader.parse();
  return { node, diagnostics: reader.diagnostic ? [reader.diagnostic] : [] };
}

function renderNode(node: JsonNode, indent: string, depth: number): string {
  if (node.kind === "raw") return node.raw;
  if (node.kind === "array") {
    if (!node.items.length) return "[]";
    const inner = node.items
      .map((item) => `${indent.repeat(depth + 1)}${renderNode(item, indent, depth + 1)}`)
      .join(",\n");
    return `[\n${inner}\n${indent.repeat(depth)}]`;
  }
  if (!node.members.length) return "{}";
  const inner = node.members
    .map(({ key, value }) => `${indent.repeat(depth + 1)}${key}: ${renderNode(value, indent, depth + 1)}`)
    .join(",\n");
  return `{\n${inner}\n${indent.repeat(depth)}}`;
}

function formatJson(source: string, options: FormatOptions = {}): FormatResult {
  const parsed = parseJson(source);
  if (!parsed.node) return { ok: false, diagnostics: parsed.diagnostics };
  const width = options.indent ?? 2;
  if (
    width !== "tab"
    && (!Number.isInteger(width) || width < 0 || width > 16)
  ) {
    return {
      ok: false,
      diagnostics: [{
        from: 0,
        to: 0,
        severity: "error",
        code: "invalid-format-options",
        message: "Размер отступа должен быть целым числом от 0 до 16 или tab",
      }],
    };
  }
  const indent = width === "tab" ? "\t" : " ".repeat(width);
  const text = renderNode(parsed.node, indent, 0);
  return { ok: true, text: options.finalNewline ? `${text}\n` : text };
}

export const jsonLanguage: LanguageDefinition<JsonLexerState> = {
  id: "json",
  name: "JSON",
  aliases: ["application/json"],
  extensions: ["json", "geojson", "har"],
  mimeTypes: ["application/json", "text/json"],
  validationLevel: "structural",
  initialState,
  statesEqual,
  stateKey: (state) => `${state.root}|${state.overflowDepth}|${state.stack?.depth ?? 0}|${state.stack?.hash ?? 0}`,
  finalize(source, endState) {
    if (endState.overflowDepth > 0 || endState.stack) {
      return [{
        from: source.length,
        to: source.length,
        severity: "error",
        code: "json-unclosed-container",
        message: `JSON-${endState.stack?.kind === "object" ? "объект" : "массив"} не закрыт`,
      }];
    }
    if (endState.root === "value") {
      return [{
        from: source.length,
        to: source.length,
        severity: "error",
        code: "json-expected-value",
        message: "Ожидалось значение JSON",
      }];
    }
    return [];
  },
  tokenizeLine: tokenizeJsonLine,
  validate: (source) => parseJson(source).diagnostics,
  format: formatJson,
};
