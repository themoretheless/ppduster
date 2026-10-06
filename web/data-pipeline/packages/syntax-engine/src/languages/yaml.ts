import type { LanguageDefinition, LineToken, TokenKind } from "../types.js";

export interface YamlLexerState {
  readonly blockIndent: number | null;
  readonly pendingBlockParentIndent: number | null;
  readonly quote: YamlQuote | null;
  readonly plainParentIndent: number | null;
}

type YamlQuote = "\"" | "'";

const BOOLEAN = /^(?:true|True|TRUE|false|False|FALSE)$/u;
const NULL = /^(?:null|Null|NULL|~)$/u;
const INTEGER = /^(?:[-+]?\d+|0o[0-7]+|0x[\da-fA-F]+)$/u;
const FLOAT = /^[+-]?(?:(?:\d+\.\d*|\.\d+)(?:[eE][+-]?\d+)?|\d+[eE][+-]?\d+)$/u;
const SPECIAL_FLOAT = /^(?:[-+]?(?:\.inf|\.Inf|\.INF)|(?:\.nan|\.NaN|\.NAN))$/u;

function initialState(): YamlLexerState {
  return Object.freeze({
    blockIndent: null,
    pendingBlockParentIndent: null,
    quote: null,
    plainParentIndent: null,
  });
}

function yamlStringEnd(
  line: string,
  from: number,
  quote: YamlQuote,
  continuation = false,
): { readonly end: number; readonly closed: boolean } {
  let index = continuation ? from : from + 1;
  while (index < line.length) {
    if (quote === '"' && line[index] === "\\") index += 2;
    else if (line[index] === quote) {
      if (quote === "'" && line[index + 1] === "'") index += 2;
      else return { end: index + 1, closed: true };
    } else index += 1;
  }
  return { end: line.length, closed: false };
}

function isSeparation(value: string | undefined): boolean {
  return value === undefined || /\s/u.test(value);
}

function isMappingColon(line: string, index: number): boolean {
  return line[index] === ":" && isSeparation(line[index + 1]);
}

function mappingColonAfter(line: string, from: number): number | null {
  let index = from;
  while (index < line.length && /\s/u.test(line[index])) index += 1;
  return isMappingColon(line, index) ? index : null;
}

function isDocumentMarker(line: string, index: number): boolean {
  return index === 0
    && (line.startsWith("---", index) || line.startsWith("...", index))
    && isSeparation(line[index + 3]);
}

function plainScalarEnd(line: string, from: number, stopAtMappingColon: boolean): number {
  let index = from;
  while (index < line.length) {
    if (line[index] === "#" && (index === from || /\s/u.test(line[index - 1]))) break;
    if (stopAtMappingColon && isMappingColon(line, index)) break;
    if ("[],{}".includes(line[index])) break;
    index += 1;
  }
  while (index > from && /\s/u.test(line[index - 1])) index -= 1;
  return index;
}

function plainScalarKind(value: string): TokenKind {
  if (BOOLEAN.test(value)) return "boolean";
  if (NULL.test(value)) return "null";
  if (INTEGER.test(value) || FLOAT.test(value) || SPECIAL_FLOAT.test(value)) return "number";
  return "text";
}

export const yamlLanguage: LanguageDefinition<YamlLexerState> = {
  id: "yaml",
  name: "YAML",
  aliases: ["yml"],
  extensions: ["yaml", "yml"],
  mimeTypes: ["application/yaml", "text/yaml", "text/x-yaml"],
  initialState,
  stateKey: (state) => [
    state.blockIndent ?? "-",
    state.pendingBlockParentIndent ?? "-",
    state.quote ?? "-",
    state.plainParentIndent ?? "-",
  ].join("|"),
  finalize(source, endState) {
    if (endState.quote === null) return [];
    return [{
      from: source.length,
      to: source.length,
      severity: "error",
      code: "yaml-unterminated-quoted-scalar",
      message: "Многострочный YAML-скаляр в кавычках не закрыт",
    }];
  },
  tokenizeLine(line, startState) {
    const tokens: LineToken[] = [];
    let current = startState as YamlLexerState;
    const indentation = line.match(/^ */u)?.[0].length ?? 0;
    const blank = /^\s*$/u.test(line);
    const push = (from: number, to: number, kind: TokenKind) => {
      if (to > from) tokens.push({ from, to, kind });
    };

    let index = 0;
    let expectKey = true;
    if (current.quote !== null) {
      const quote = current.quote;
      const result = yamlStringEnd(line, 0, quote, true);
      push(0, result.end, "string");
      if (!result.closed) return { tokens, state: current };
      index = result.end;
      expectKey = false;
      current = initialState();
    }

    if (current.pendingBlockParentIndent !== null) {
      if (blank || indentation > current.pendingBlockParentIndent) {
        push(0, line.length, "string");
        return {
          tokens,
          state: Object.freeze({
            blockIndent: blank ? null : indentation,
            pendingBlockParentIndent: blank ? current.pendingBlockParentIndent : null,
            quote: null,
            plainParentIndent: null,
          }),
        };
      }
      current = initialState();
    }
    if (current.blockIndent !== null) {
      if (blank || indentation >= current.blockIndent) {
        push(0, line.length, "string");
        return { tokens, state: current };
      }
      current = initialState();
    }

    if (current.plainParentIndent !== null) {
      if (blank) {
        push(0, line.length, "whitespace");
        return { tokens, state: current };
      }
      if (indentation > current.plainParentIndent) {
        push(0, indentation, "whitespace");
        if (line[indentation] === "#") push(indentation, line.length, "comment");
        else {
          let commentAt = -1;
          for (let at = indentation; at < line.length; at += 1) {
            if (line[at] === "#" && /\s/u.test(line[at - 1] ?? "")) {
              commentAt = at;
              break;
            }
          }
          const scalarEnd = commentAt < 0 ? line.length : commentAt;
          let textEnd = scalarEnd;
          while (textEnd > indentation && /\s/u.test(line[textEnd - 1])) textEnd -= 1;
          push(indentation, textEnd, "text");
          push(textEnd, scalarEnd, "whitespace");
          if (commentAt >= 0) push(commentAt, line.length, "comment");
        }
        return { tokens, state: current };
      }
      current = initialState();
    }

    let pendingBlock = false;
    let plainScalarValue = false;
    let valueParentIndent = indentation;
    while (index < line.length) {
      if (/\s/u.test(line[index])) {
        const from = index++;
        while (index < line.length && /\s/u.test(line[index])) index += 1;
        push(from, index, "whitespace");
        continue;
      }
      if (line[index] === "#") {
        push(index, line.length, "comment");
        break;
      }
      if (isDocumentMarker(line, index)) {
        push(index, index + 3, "directive");
        index += 3;
        continue;
      }
      if (line[index] === "%") {
        push(index, line.length, "directive");
        break;
      }
      if (line[index] === '"' || line[index] === "'") {
        const quote = line[index] as YamlQuote;
        const result = yamlStringEnd(line, index, quote);
        const kind = result.closed && expectKey && mappingColonAfter(line, result.end) !== null
          ? "property"
          : "string";
        push(index, result.end, kind);
        if (kind === "property") valueParentIndent = index;
        index = result.end;
        if (!result.closed) {
          return {
            tokens,
            state: Object.freeze({
              blockIndent: null,
              pendingBlockParentIndent: null,
              quote,
              plainParentIndent: null,
            }),
          };
        }
        expectKey = false;
        continue;
      }
      if (line.startsWith("!<", index)) {
        const from = index;
        const close = line.indexOf(">", index + 2);
        index = close < 0 ? line.length : close + 1;
        push(from, index, "type");
        continue;
      }
      if ("&*!".includes(line[index])) {
        const from = index++;
        while (index < line.length && !/[\s,\[\]{}:#]/u.test(line[index])) index += 1;
        push(from, index, line[from] === "!" ? "type" : "decorator");
        continue;
      }
      if ("[]{}?,".includes(line[index]) || (line[index] === "-" && isSeparation(line[index + 1]))) {
        push(index, index + 1, "punctuation");
        expectKey = line[index] !== "]" && line[index] !== "}";
        index += 1;
        continue;
      }
      if (isMappingColon(line, index)) {
        push(index, index + 1, "punctuation");
        expectKey = false;
        index += 1;
        continue;
      }
      if (line[index] === "|" || line[index] === ">") {
        const from = index++;
        while (index < line.length && /[+\-1-9]/u.test(line[index])) index += 1;
        push(from, index, "operator");
        pendingBlock = true;
        continue;
      }

      const from = index;
      index = plainScalarEnd(line, from, expectKey);
      if (index === from) {
        push(index, index + 1, "text");
        plainScalarValue = true;
        index += 1;
        expectKey = false;
        continue;
      }
      const value = line.slice(from, index);
      const kind: TokenKind = expectKey && mappingColonAfter(line, index) !== null
        ? "property"
        : plainScalarKind(value);
      push(from, index, kind);
      if (kind === "property") valueParentIndent = from;
      else plainScalarValue = true;
      expectKey = false;
    }

    return {
      tokens,
      state: pendingBlock
        ? Object.freeze({
          blockIndent: null,
          pendingBlockParentIndent: indentation,
          quote: null,
          plainParentIndent: null,
        })
        : plainScalarValue
          ? Object.freeze({
            blockIndent: null,
            pendingBlockParentIndent: null,
            quote: null,
            plainParentIndent: valueParentIndent,
          })
          : current,
    };
  },
};
