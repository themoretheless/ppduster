import type {
  LanguageDefinition,
  LineDiagnostic,
  LineToken,
  TokenKind,
} from "../types.js";

export type StringEscape = "backslash" | "double" | "none";

export interface StringRule {
  readonly open: string | RegExp;
  readonly close?: string | ((opening: string, match: RegExpMatchArray | null) => string);
  readonly multiline?: boolean;
  readonly lineContinuation?: boolean;
  readonly escape?: StringEscape;
  readonly kind?: TokenKind;
}

export interface CodeLanguageConfig {
  readonly id: string;
  readonly name: string;
  readonly aliases?: readonly string[];
  readonly extensions?: readonly string[];
  readonly mimeTypes?: readonly string[];
  readonly keywords: readonly string[];
  readonly types?: readonly string[];
  readonly builtins?: readonly string[];
  readonly booleans?: readonly string[];
  readonly nulls?: readonly string[];
  readonly lineComments?: readonly string[];
  readonly lineStartComments?: readonly string[];
  readonly blockComment?: { readonly open: string; readonly close: string; readonly nested?: boolean };
  readonly strings?: readonly StringRule[];
  readonly numberPattern?: RegExp;
  readonly operators?: readonly string[];
  readonly punctuation?: string;
  readonly caseInsensitive?: boolean;
  readonly directiveMarker?: string;
  readonly decorators?: boolean;
  readonly decoratorsAtLineStart?: boolean;
  readonly macroSuffix?: string;
  readonly lifetimeIdentifiers?: boolean;
  readonly rawIdentifierPrefix?: string;
  readonly regexLiterals?: boolean;
  readonly extraIdentifierStart?: string;
  readonly extraIdentifierPart?: string;
}

export interface CodeLexerState {
  readonly mode: "root" | "block-comment" | "string";
  readonly blockDepth: number;
  readonly close: string;
  readonly escape: StringEscape;
  readonly stringKind: TokenKind;
  readonly stringContinuation: "" | "multiline" | "escaped-line";
  readonly bracketDepth: number;
  readonly explicitLineContinuation: boolean;
  readonly canStartExpression: boolean;
  readonly parenDepth: number;
  readonly controlParenDepth: number;
  readonly pendingControlParen: boolean;
  readonly afterMemberAccess: boolean;
}

const DEFAULT_NUMBER = /^(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*|0[bB][01](?:_?[01])*|0[oO][0-7](?:_?[0-7])*|(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?)/;
const DEFAULT_OPERATORS = [
  ">>>=", "<<=", ">>=", "===", "!==", "??=", "&&=", "||=", "**=", "=>", "==", "!=",
  "<=", ">=", "++", "--", "&&", "||", "??", "?.", "::", "->", "**", "<<", ">>", "+=",
  "-=", "*=", "/=", "%=", "&=", "|=", "^=", "..=", "...", "..", "+", "-", "*", "/", "%",
  "=", "<", ">", "!", "~", "&", "|", "^", "?", ":",
] as const;

function immutableState(state: CodeLexerState): CodeLexerState {
  return Object.freeze(state);
}

function initialState(): CodeLexerState {
  return immutableState({
    mode: "root",
    blockDepth: 0,
    close: "",
    escape: "none",
    stringKind: "string",
    stringContinuation: "",
    bracketDepth: 0,
    explicitLineContinuation: false,
    canStartExpression: true,
    parenDepth: 0,
    controlParenDepth: 0,
    pendingControlParen: false,
    afterMemberAccess: false,
  });
}

function normalizeWord(value: string, insensitive: boolean): string {
  return insensitive ? value.toLowerCase() : value;
}

function anchored(pattern: RegExp): RegExp {
  const flags = pattern.flags.replaceAll("g", "").replaceAll("y", "");
  return new RegExp(`^(?:${pattern.source})`, flags);
}

function stringOpening(rule: StringRule, source: string, index: number): {
  readonly opening: string;
  readonly match: RegExpMatchArray | null;
} | null {
  if (typeof rule.open === "string") {
    return source.startsWith(rule.open, index) ? { opening: rule.open, match: null } : null;
  }
  const match = source.slice(index).match(rule.open);
  return match?.[0] ? { opening: match[0], match } : null;
}

function findStringEnd(
  line: string,
  from: number,
  close: string,
  escape: StringEscape,
): number {
  let index = from;
  while (index < line.length) {
    if (escape === "backslash" && line[index] === "\\") {
      index += Math.min(2, line.length - index);
      continue;
    }
    if (line.startsWith(close, index)) {
      if (escape === "double" && line.startsWith(close + close, index)) {
        index += close.length * 2;
        continue;
      }
      return index + close.length;
    }
    index += 1;
  }
  return -1;
}

function hasTrailingLineContinuation(line: string): boolean {
  let backslashes = 0;
  for (let index = line.length - 1; index >= 0 && line[index] === "\\"; index -= 1) {
    backslashes += 1;
  }
  return backslashes % 2 === 1;
}

function findRegexEnd(line: string, from: number): number {
  let escaped = false;
  let characterClass = false;
  for (let index = from + 1; index < line.length; index += 1) {
    const character = line[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (character === "\\") {
      escaped = true;
      continue;
    }
    if (character === "[") characterClass = true;
    else if (character === "]") characterClass = false;
    else if (character === "/" && !characterClass) {
      let end = index + 1;
      while (/[a-z]/i.test(line[end] ?? "")) end += 1;
      return end;
    }
  }
  return -1;
}

function identifierPattern(
  extraStart: string,
  extraPart: string,
): RegExp {
  const escapedStart = extraStart.replace(/[\\\]\-^]/g, "\\$&");
  const escapedPart = extraPart.replace(/[\\\]\-^]/g, "\\$&");
  return new RegExp(
    `^[_${escapedStart}\\p{ID_Start}][_${escapedPart}\\u200c\\u200d\\p{ID_Continue}]*`,
    "u",
  );
}

function identifierAt(
  line: string,
  index: number,
  pattern: RegExp,
): string {
  return line.slice(index).match(pattern)?.[0] ?? "";
}

function nextNonWhitespace(line: string, from: number): string {
  for (let index = from; index < line.length; index += 1) {
    if (!/\s/u.test(line[index])) return line[index];
  }
  return "";
}

function expressionAfter(kind: TokenKind, text: string): boolean {
  if (["identifier", "function", "number", "string", "regexp", "boolean", "null", "type"].includes(kind)) {
    return false;
  }
  if (kind === "punctuation" && [")", "]", "}"].includes(text)) return false;
  if (kind === "keyword" && ["this", "super", "self"].includes(text)) return false;
  return true;
}

const CONTROL_PAREN_KEYWORDS = new Set(["catch", "for", "if", "switch", "while", "with"]);

export function createCodeLanguage(config: CodeLanguageConfig): LanguageDefinition<CodeLexerState> {
  const nonEmpty = (value: string, label: string) => {
    if (!value.length) throw new RangeError(`${label} не может быть пустым`);
  };
  for (const marker of config.lineComments ?? []) nonEmpty(marker, "Маркер комментария");
  for (const marker of config.lineStartComments ?? []) nonEmpty(marker, "Маркер комментария");
  for (const operator of config.operators ?? []) nonEmpty(operator, "Оператор");
  if (config.blockComment) {
    nonEmpty(config.blockComment.open, "Начало блочного комментария");
    nonEmpty(config.blockComment.close, "Конец блочного комментария");
  }
  if (config.directiveMarker !== undefined) nonEmpty(config.directiveMarker, "Маркер директивы");
  if (config.macroSuffix !== undefined) nonEmpty(config.macroSuffix, "Суффикс макроса");
  if (config.rawIdentifierPrefix !== undefined) nonEmpty(config.rawIdentifierPrefix, "Префикс identifier");
  for (const rule of config.strings ?? []) {
    if (typeof rule.open === "string") nonEmpty(rule.open, "Начало строки");
    if (typeof rule.close === "string") nonEmpty(rule.close, "Конец строки");
  }

  const insensitive = config.caseInsensitive === true;
  const keywords = new Set(config.keywords.map((word) => normalizeWord(word, insensitive)));
  const types = new Set((config.types ?? []).map((word) => normalizeWord(word, insensitive)));
  const builtins = new Set((config.builtins ?? []).map((word) => normalizeWord(word, insensitive)));
  const booleans = new Set((config.booleans ?? ["true", "false"]).map((word) => normalizeWord(word, insensitive)));
  const nulls = new Set((config.nulls ?? ["null"]).map((word) => normalizeWord(word, insensitive)));
  const lineComments = [...(config.lineComments ?? ["//"])].sort((left, right) => right.length - left.length);
  const lineStartComments = [...(config.lineStartComments ?? [])].sort((left, right) => right.length - left.length);
  const strings = [...(config.strings ?? [
    { open: '"', escape: "backslash" as const },
    { open: "'", escape: "backslash" as const },
  ])].map((rule) => ({
    ...rule,
    open: typeof rule.open === "string" ? rule.open : anchored(rule.open),
  }));
  const operators = [...(config.operators ?? DEFAULT_OPERATORS)].sort((left, right) => right.length - left.length);
  const punctuation = config.punctuation ?? "{}[]();,.";
  const numberPattern = anchored(config.numberPattern ?? DEFAULT_NUMBER);
  const identifiers = identifierPattern(
    config.extraIdentifierStart ?? "",
    config.extraIdentifierPart ?? "",
  );
  const tracksLogicalContinuation = config.decoratorsAtLineStart === true;
  const tracksRegexContext = config.regexLiterals === true;

  return {
    id: config.id,
    name: config.name,
    aliases: config.aliases,
    extensions: config.extensions,
    mimeTypes: config.mimeTypes,
    initialState,
    stateKey: (state) => [
      state.mode,
      state.blockDepth,
      state.close,
      state.escape,
      state.stringKind,
      state.stringContinuation,
      tracksLogicalContinuation ? state.bracketDepth : 0,
      tracksLogicalContinuation && state.explicitLineContinuation ? 1 : 0,
      tracksRegexContext && state.canStartExpression ? 1 : 0,
      tracksRegexContext ? state.parenDepth : 0,
      tracksRegexContext ? state.controlParenDepth : 0,
      tracksRegexContext && state.pendingControlParen ? 1 : 0,
      tracksRegexContext && state.afterMemberAccess ? 1 : 0,
    ].join("|"),
    tokenizeLine(line, startState) {
      const tokens: LineToken[] = [];
      const diagnostics: LineDiagnostic[] = [];
      let state = startState as CodeLexerState;
      const startsInLogicalContinuation = tracksLogicalContinuation
        && (state.bracketDepth > 0 || state.explicitLineContinuation);
      if (tracksLogicalContinuation && state.mode === "root" && state.explicitLineContinuation) {
        state = immutableState({ ...state, explicitLineContinuation: false });
      }
      let index = 0;

      const push = (from: number, to: number, kind: TokenKind) => {
        if (to > from) tokens.push({ from, to, kind });
      };

      if (state.mode === "block-comment") {
        const start = 0;
        let depth = state.blockDepth;
        while (index < line.length) {
          if (config.blockComment?.nested && line.startsWith(config.blockComment.open, index)) {
            depth += 1;
            index += config.blockComment.open.length;
          } else if (config.blockComment && line.startsWith(config.blockComment.close, index)) {
            depth -= 1;
            index += config.blockComment.close.length;
            if (depth === 0) break;
          } else index += 1;
        }
        push(start, index, "comment");
        if (depth > 0) {
          return {
            tokens,
            diagnostics,
            state: immutableState({ ...state, blockDepth: depth }),
          };
        }
        state = immutableState({ ...state, mode: "root", blockDepth: 0 });
      } else if (state.mode === "string") {
        const end = findStringEnd(line, 0, state.close, state.escape);
        if (end < 0) {
          push(0, line.length, state.stringKind);
          if (state.stringContinuation !== "escaped-line" || hasTrailingLineContinuation(line)) {
            return { tokens, diagnostics, state };
          }
          diagnostics.push({
            from: 0,
            to: line.length,
            severity: "error",
            code: "unterminated-string",
            message: "Незакрытая строка",
          });
          return {
            tokens,
            diagnostics,
            state: immutableState({
              ...state,
              mode: "root",
              close: "",
              escape: "none",
              stringContinuation: "",
            }),
          };
        }
        push(0, end, state.stringKind);
        index = end;
        state = immutableState({
          ...state,
          mode: "root",
          close: "",
          escape: "none",
          stringContinuation: "",
          canStartExpression: false,
          pendingControlParen: false,
          afterMemberAccess: false,
        });
      }

      while (index < line.length) {
        const character = line[index];
        if (/\s/u.test(character)) {
          const from = index++;
          while (index < line.length && /\s/u.test(line[index])) index += 1;
          push(from, index, "whitespace");
          continue;
        }

        if (
          config.directiveMarker
          && character === config.directiveMarker
          && line.slice(0, index).trim().length === 0
        ) {
          push(index, line.length, "directive");
          index = line.length;
          continue;
        }

        const lineStartComment = lineStartComments.find((marker) => line.startsWith(marker, index));
        if (lineStartComment && line.slice(0, index).trim().length === 0) {
          push(index, line.length, "comment");
          index = line.length;
          continue;
        }

        const lineComment = lineComments.find((marker) => line.startsWith(marker, index));
        if (lineComment) {
          push(index, line.length, "comment");
          index = line.length;
          continue;
        }

        if (config.blockComment && line.startsWith(config.blockComment.open, index)) {
          const from = index;
          let depth = 1;
          index += config.blockComment.open.length;
          while (index < line.length && depth > 0) {
            if (config.blockComment.nested && line.startsWith(config.blockComment.open, index)) {
              depth += 1;
              index += config.blockComment.open.length;
            } else if (line.startsWith(config.blockComment.close, index)) {
              depth -= 1;
              index += config.blockComment.close.length;
            } else index += 1;
          }
          push(from, index, "comment");
          if (depth > 0) {
            state = immutableState({ ...state, mode: "block-comment", blockDepth: depth });
            break;
          }
          continue;
        }

        if (config.lifetimeIdentifiers && character === "'") {
          const lifetime = identifierAt(line, index + 1, identifiers);
          if (lifetime && line[index + 1 + lifetime.length] !== "'") {
            push(index, index + 1 + lifetime.length, "decorator");
            index += 1 + lifetime.length;
            state = immutableState({
              ...state,
              canStartExpression: true,
              pendingControlParen: false,
              afterMemberAccess: false,
            });
            continue;
          }
        }

        let matchedString = false;
        for (const rule of strings) {
          const opening = stringOpening(rule, line, index);
          if (!opening) continue;
          const close = typeof rule.close === "function"
            ? rule.close(opening.opening, opening.match)
            : rule.close ?? (typeof rule.open === "string" ? rule.open.at(-1) ?? rule.open : opening.opening);
          if (!close.length) throw new RangeError("Конец строки не может быть пустым");
          const escape = rule.escape ?? "backslash";
          const end = findStringEnd(line, index + opening.opening.length, close, escape);
          const kind = rule.kind ?? "string";
          if (end >= 0) {
            push(index, end, kind);
            state = immutableState({
              ...state,
              canStartExpression: false,
              pendingControlParen: false,
              afterMemberAccess: false,
            });
            index = end;
          } else {
            push(index, line.length, kind);
            if (rule.multiline || (rule.lineContinuation && hasTrailingLineContinuation(line))) {
              state = immutableState({
                ...state,
                mode: "string",
                close,
                escape,
                stringKind: kind,
                stringContinuation: rule.multiline ? "multiline" : "escaped-line",
                canStartExpression: false,
                pendingControlParen: false,
                afterMemberAccess: false,
              });
            } else {
              diagnostics.push({
                from: index,
                to: line.length,
                severity: "error",
                code: "unterminated-string",
                message: "Незакрытая строка",
              });
            }
            index = line.length;
          }
          matchedString = true;
          break;
        }
        if (matchedString) continue;

        if (config.regexLiterals && character === "/" && state.canStartExpression) {
          const end = findRegexEnd(line, index);
          if (end > 0) {
            push(index, end, "regexp");
            state = immutableState({
              ...state,
              canStartExpression: false,
              pendingControlParen: false,
              afterMemberAccess: false,
            });
            index = end;
            continue;
          }
        }

        if (
          config.decorators
          && character === "@"
          && (
            !config.decoratorsAtLineStart
            || (!startsInLogicalContinuation && line.slice(0, index).trim().length === 0)
          )
        ) {
          const identifier = identifierAt(
            line,
            index + 1,
            identifiers,
          );
          if (identifier) {
            push(index, index + 1 + identifier.length, "decorator");
            index += 1 + identifier.length;
            state = immutableState({
              ...state,
              canStartExpression: true,
              pendingControlParen: false,
              afterMemberAccess: false,
            });
            continue;
          }
        }

        if (config.rawIdentifierPrefix && line.startsWith(config.rawIdentifierPrefix, index)) {
          const identifier = identifierAt(line, index + config.rawIdentifierPrefix.length, identifiers);
          if (identifier) {
            push(index, index + config.rawIdentifierPrefix.length + identifier.length, "identifier");
            index += config.rawIdentifierPrefix.length + identifier.length;
            state = immutableState({
              ...state,
              canStartExpression: false,
              pendingControlParen: false,
              afterMemberAccess: false,
            });
            continue;
          }
        }

        const number = line.slice(index).match(numberPattern)?.[0] ?? "";
        if (number && (/\d/.test(character) || (character === "." && /\d/.test(line[index + 1] ?? "")))) {
          push(index, index + number.length, "number");
          index += number.length;
          state = immutableState({
            ...state,
            canStartExpression: false,
            pendingControlParen: false,
            afterMemberAccess: false,
          });
          continue;
        }

        const identifier = identifierAt(
          line,
          index,
          identifiers,
        );
        if (identifier) {
          const normalized = normalizeWord(identifier, insensitive);
          const suffixAt = index + identifier.length;
          const followsMemberAccess = tracksRegexContext && state.afterMemberAccess;
          const hasMacroSuffix = Boolean(
            config.macroSuffix
            && line.startsWith(config.macroSuffix, suffixAt)
            && (
              "([{".includes(nextNonWhitespace(line, suffixAt + config.macroSuffix.length))
              || normalized === "macro_rules"
            )
          );
          const to = index + identifier.length + (hasMacroSuffix ? config.macroSuffix!.length : 0);
          const kind: TokenKind = followsMemberAccess
            ? "identifier"
            : hasMacroSuffix
              ? "macro"
              : booleans.has(normalized)
                ? "boolean"
                : nulls.has(normalized)
                  ? "null"
                  : types.has(normalized) || builtins.has(normalized)
                    ? "type"
                    : keywords.has(normalized)
                      ? "keyword"
                      : nextNonWhitespace(line, to) === "("
                        ? "function"
                        : "identifier";
          push(index, to, kind);
          const continuesControlHeader = tracksRegexContext
            && state.pendingControlParen
            && normalized === "await";
          state = immutableState({
            ...state,
            canStartExpression: followsMemberAccess ? false : expressionAfter(kind, normalized),
            pendingControlParen: tracksRegexContext
              && !followsMemberAccess
              && (CONTROL_PAREN_KEYWORDS.has(normalized) || continuesControlHeader),
            afterMemberAccess: false,
          });
          index = to;
          continue;
        }

        const operator = operators.find((candidate) => line.startsWith(candidate, index));
        if (operator) {
          const wasExpectingExpression = state.canStartExpression;
          push(index, index + operator.length, "operator");
          index += operator.length;
          // ++/-- can be prefix or postfix. After a postfix operator the
          // following slash is division, not the start of a regexp literal.
          state = immutableState({
            ...state,
            canStartExpression: operator === "++" || operator === "--"
              ? wasExpectingExpression
              : true,
            pendingControlParen: false,
            afterMemberAccess: tracksRegexContext && operator === "?.",
          });
          continue;
        }

        if (punctuation.includes(character)) {
          push(index, index + 1, "punctuation");
          const bracketDepth = tracksLogicalContinuation
            ? "([{".includes(character)
              ? state.bracketDepth + 1
              : ")]}".includes(character)
                ? Math.max(0, state.bracketDepth - 1)
                : state.bracketDepth
            : state.bracketDepth;
          const previousParenDepth = state.parenDepth;
          const parenDepth = tracksRegexContext
            ? character === "("
              ? previousParenDepth + 1
              : character === ")"
                ? Math.max(0, previousParenDepth - 1)
                : previousParenDepth
            : previousParenDepth;
          const opensControlHeader = tracksRegexContext
            && character === "("
            && state.pendingControlParen;
          const closesControlHeader = tracksRegexContext
            && character === ")"
            && state.controlParenDepth > 0
            && previousParenDepth === state.controlParenDepth;
          state = immutableState({
            ...state,
            bracketDepth,
            parenDepth,
            controlParenDepth: opensControlHeader
              ? parenDepth
              : closesControlHeader
                ? 0
                : state.controlParenDepth,
            canStartExpression: closesControlHeader
              ? true
              : expressionAfter("punctuation", character),
            pendingControlParen: false,
            afterMemberAccess: tracksRegexContext && character === ".",
          });
          index += 1;
          continue;
        }

        const width = (line.codePointAt(index) ?? 0) > 0xffff ? 2 : 1;
        push(index, index + width, character.charCodeAt(0) < 32 ? "invalid" : "operator");
        if (character.charCodeAt(0) < 32) {
          diagnostics.push({
            from: index,
            to: index + 1,
            severity: "error",
            code: "control-character",
            message: "Недопустимый управляющий символ",
          });
        }
        if (character === "\\" && line.slice(index + width).trim().length === 0) {
          state = immutableState({
            ...state,
            explicitLineContinuation: tracksLogicalContinuation,
            pendingControlParen: false,
            afterMemberAccess: false,
          });
        } else {
          state = immutableState({
            ...state,
            canStartExpression: true,
            pendingControlParen: false,
            afterMemberAccess: false,
          });
        }
        index += width;
      }

      return { tokens, diagnostics, state };
    },
    finalize(source, endState) {
      if (endState.mode === "root") return [];
      const label = endState.mode === "block-comment" ? "Многострочный комментарий" : "Многострочная строка";
      return [{
        from: source.length,
        to: source.length,
        severity: "error",
        code: endState.mode === "block-comment" ? "unterminated-comment" : "unterminated-string",
        message: `${label} не закрыт${endState.mode === "block-comment" ? "" : "а"}`,
      }];
    },
  };
}
