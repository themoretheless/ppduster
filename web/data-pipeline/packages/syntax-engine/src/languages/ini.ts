import type { LanguageDefinition, LineToken, TokenKind } from "../types.js";

interface IniLexerState {
  readonly continuation: boolean;
}

const initialState = (): IniLexerState => Object.freeze({ continuation: false });
const NUMBER = /^[+-]?(?:0x[\da-f]+|\d+(?:\.\d+)?(?:e[+-]?\d+)?)$/iu;
const BOOLEAN = new Set(["true", "false", "yes", "no", "on", "off", "enabled", "disabled"]);
const NULL = new Set(["null", "none", "nil"]);

function isEscaped(line: string, at: number): boolean {
  let backslashes = 0;
  for (let index = at - 1; index >= 0 && line[index] === "\\"; index -= 1) backslashes += 1;
  return backslashes % 2 === 1;
}

export const iniLanguage: LanguageDefinition<IniLexerState> = {
  id: "ini",
  name: "INI",
  extensions: ["ini", "cfg", "conf", "editorconfig"],
  mimeTypes: ["text/x-ini"],
  initialState,
  stateKey: (state) => state.continuation ? "1" : "0",
  tokenizeLine(line, startState) {
    const tokens: LineToken[] = [];
    const push = (from: number, to: number, kind: TokenKind) => {
      if (to > from) tokens.push({ from, to, kind });
    };
    const indentation = line.match(/^\s*/u)?.[0].length ?? 0;
    if (indentation) push(0, indentation, "whitespace");
    if (startState.continuation) {
      push(indentation, line.length, "string");
      return { tokens, state: Object.freeze({ continuation: /(?<!\\)(?:\\\\)*\\\s*$/u.test(line) }) };
    }
    if (line[indentation] === ";" || line[indentation] === "#") {
      push(indentation, line.length, "comment");
      return { tokens, state: initialState() };
    }
    const section = line.slice(indentation).match(/^\[[^\]\r\n]+\]/u)?.[0];
    if (section) {
      push(indentation, indentation + section.length, "section");
      const rest = indentation + section.length;
      if (rest < line.length) push(rest, line.length, /^\s*[;#]/u.test(line.slice(rest)) ? "comment" : "text");
      return { tokens, state: initialState() };
    }

    const separator = line.slice(indentation).search(/[=:]/u);
    if (separator < 0) {
      push(indentation, line.length, line.trim() ? "text" : "whitespace");
      return { tokens, state: initialState() };
    }
    const separatorAt = indentation + separator;
    let keyEnd = separatorAt;
    while (keyEnd > indentation && /\s/u.test(line[keyEnd - 1])) keyEnd -= 1;
    push(indentation, keyEnd, "property");
    push(keyEnd, separatorAt, "whitespace");
    push(separatorAt, separatorAt + 1, "operator");

    let index = separatorAt + 1;
    while (index < line.length && /\s/u.test(line[index])) index += 1;
    push(separatorAt + 1, index, "whitespace");
    const valueFrom = index;
    let quote = "";
    while (index < line.length) {
      if (!quote && (line[index] === ";" || line[index] === "#") && (index === valueFrom || /\s/u.test(line[index - 1]))) break;
      if ((line[index] === '"' || line[index] === "'") && !isEscaped(line, index)) {
        quote = quote === line[index] ? "" : quote || line[index];
      }
      index += 1;
    }
    let valueEnd = index;
    while (valueEnd > valueFrom && /\s/u.test(line[valueEnd - 1])) valueEnd -= 1;
    const rawValue = line.slice(valueFrom, valueEnd);
    const normalized = rawValue.toLowerCase();
    const valueKind: TokenKind = /^(['"]).*\1$/u.test(rawValue)
      ? "string"
      : BOOLEAN.has(normalized)
        ? "boolean"
        : NULL.has(normalized)
          ? "null"
          : NUMBER.test(rawValue)
            ? "number"
            : "text";
    push(valueFrom, valueEnd, valueKind);
    push(valueEnd, index, "whitespace");
    if (index < line.length) push(index, line.length, "comment");
    return {
      tokens,
      state: Object.freeze({ continuation: /(?<!\\)(?:\\\\)*\\\s*$/u.test(line.slice(0, index)) }),
    };
  },
};
