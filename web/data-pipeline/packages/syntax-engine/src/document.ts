import type {
  ChangeOptions,
  IncrementalUpdate,
  LanguageDefinition,
  LineDiagnostic,
  LineToken,
  SyntaxDiagnostic,
  SyntaxToken,
  TextChange,
  TextPosition,
  TokenizedLine,
} from "./types.js";

interface SourceLine {
  readonly text: string;
  readonly lineBreak: string;
}

interface CachedLine<State> {
  readonly view: TokenizedLine;
  readonly startState: State;
  readonly startStateKey: string;
  readonly endState: State;
}

let nextLineId = 1;
let nextStateId = 1;
const stateIds = new WeakMap<object, number>();

export function computeTextChange(before: string, after: string): TextChange | null {
  if (before === after) return null;
  let prefix = 0;
  const sharedLength = Math.min(before.length, after.length);
  while (prefix < sharedLength && before.charCodeAt(prefix) === after.charCodeAt(prefix)) prefix += 1;

  let beforeSuffix = before.length;
  let afterSuffix = after.length;
  while (
    beforeSuffix > prefix
    && afterSuffix > prefix
    && before.charCodeAt(beforeSuffix - 1) === after.charCodeAt(afterSuffix - 1)
  ) {
    beforeSuffix -= 1;
    afterSuffix -= 1;
  }

  return { from: prefix, to: beforeSuffix, insert: after.slice(prefix, afterSuffix) };
}

function splitLines(source: string): SourceLine[] {
  const lines: SourceLine[] = [];
  let start = 0;

  for (let index = 0; index < source.length; index += 1) {
    const character = source.charCodeAt(index);
    if (character !== 10 && character !== 13) continue;

    const lineBreak = character === 13 && source.charCodeAt(index + 1) === 10 ? "\r\n" : source[index];
    lines.push({ text: source.slice(start, index), lineBreak });
    index += lineBreak.length - 1;
    start = index + 1;
  }

  lines.push({ text: source.slice(start), lineBreak: "" });
  return lines;
}

function sameSourceLine(left: SourceLine | undefined, right: SourceLine | undefined): boolean {
  return left?.text === right?.text && left?.lineBreak === right?.lineBreak;
}

function replaceArraySegment<Value>(
  target: Value[],
  start: number,
  deleteCount: number,
  values: readonly Value[],
): void {
  // Avoid an engine argument-count limit when one edit pastes many lines.
  const CHUNK_SIZE = 8_192;
  if (values.length <= CHUNK_SIZE) {
    target.splice(start, deleteCount, ...values);
    return;
  }
  target.splice(start, deleteCount);
  for (let offset = 0; offset < values.length; offset += CHUNK_SIZE) {
    target.splice(start + offset, 0, ...values.slice(offset, offset + CHUNK_SIZE));
  }
}

function normalizeTokens(line: string, tokens: readonly LineToken[]): readonly LineToken[] {
  const normalized: LineToken[] = [];
  let cursor = 0;
  const immutableToken = (token: LineToken): LineToken => Object.freeze(token.modifiers
    ? { ...token, modifiers: Object.freeze([...token.modifiers]) }
    : { ...token });

  for (const token of tokens) {
    if (
      !Number.isInteger(token.from)
      || !Number.isInteger(token.to)
      || token.from < cursor
      || token.to <= token.from
      || token.to > line.length
    ) {
      throw new RangeError(`Некорректный диапазон токена ${token.from}:${token.to}`);
    }
    if (token.from > cursor) normalized.push(Object.freeze({ from: cursor, to: token.from, kind: "text" }));
    normalized.push(immutableToken(token));
    cursor = token.to;
  }

  if (cursor < line.length) normalized.push(Object.freeze({ from: cursor, to: line.length, kind: "text" }));
  return normalized;
}

function normalizeDiagnostics(
  line: string,
  diagnostics: readonly LineDiagnostic[] | undefined,
): readonly LineDiagnostic[] {
  if (!diagnostics?.length) return [];
  return diagnostics.map((diagnostic) => {
    if (!Number.isInteger(diagnostic.from) || !Number.isInteger(diagnostic.to)) {
      throw new RangeError(`Некорректный диапазон диагностики ${diagnostic.from}:${diagnostic.to}`);
    }
    const from = Math.max(0, Math.min(line.length, diagnostic.from));
    return Object.freeze({
      ...diagnostic,
      from,
      to: Math.max(from, Math.min(line.length, diagnostic.to)),
    });
  });
}

function defaultStateKey(state: unknown): string {
  if (state === null) return "null";
  const primitive = typeof state;
  if (primitive !== "object") return `${primitive}:${String(state)}`;
  let id = stateIds.get(state as object);
  if (id === undefined) {
    id = nextStateId++;
    stateIds.set(state as object, id);
  }
  return `object:${id}`;
}

function safeErrorMessage(error: unknown, fallback: string): string {
  try {
    return error instanceof Error && typeof error.message === "string"
      ? error.message
      : fallback;
  } catch {
    return fallback;
  }
}

export class SyntaxDocument<State = unknown> {
  readonly language: LanguageDefinition<State>;
  private source: string;
  private sourceLines: SourceLine[];
  private cache: Array<CachedLine<State> | undefined>;
  private lineStarts: number[];
  private documentVersion = 1;
  private readonly rootState: State;

  constructor(
    source: string,
    language: LanguageDefinition<State>,
  ) {
    this.language = language;
    this.rootState = language.initialState();
    this.source = source;
    this.sourceLines = splitLines(source);
    this.cache = new Array(this.sourceLines.length);
    this.lineStarts = [];
    this.rebuildLineStarts();
    this.retokenizeFrom(0);
  }

  get text(): string {
    return this.source;
  }

  get version(): number {
    return this.documentVersion;
  }

  get lineCount(): number {
    return this.sourceLines.length;
  }

  get lines(): readonly TokenizedLine[] {
    return this.cache.map((line) => {
      if (!line) throw new Error("Внутренняя ошибка кэша токенизации");
      return line.view;
    });
  }

  line(index: number): TokenizedLine {
    const line = this.cache[index];
    if (!line) throw new RangeError(`Строка ${index} не существует`);
    return line.view;
  }

  positionAt(offset: number): TextPosition {
    this.assertOffset(offset);
    const line = this.lineIndexAt(offset);
    return {
      line,
      column: Math.min(offset - this.lineStarts[line], this.sourceLines[line].text.length),
    };
  }

  offsetAt(position: TextPosition): number {
    if (!Number.isInteger(position.line) || position.line < 0 || position.line >= this.lineCount) {
      throw new RangeError(`Строка ${position.line} не существует`);
    }
    const sourceLine = this.sourceLines[position.line];
    if (
      !Number.isInteger(position.column)
      || position.column < 0
      || position.column > sourceLine.text.length
    ) {
      throw new RangeError(`Колонка ${position.column} не существует`);
    }
    return this.lineStarts[position.line] + position.column;
  }

  tokens(): readonly SyntaxToken[] {
    const tokens: SyntaxToken[] = [];
    for (let lineIndex = 0; lineIndex < this.sourceLines.length; lineIndex += 1) {
      const sourceLine = this.sourceLines[lineIndex];
      const cached = this.cache[lineIndex];
      if (!cached) continue;
      const lineStart = this.lineStarts[lineIndex];
      for (const token of cached.view.tokens) {
        tokens.push({
          ...token,
          from: lineStart + token.from,
          to: lineStart + token.to,
          line: lineIndex,
        });
      }
      if (sourceLine.lineBreak) {
        tokens.push({
          from: lineStart + sourceLine.text.length,
          to: lineStart + sourceLine.text.length + sourceLine.lineBreak.length,
          line: lineIndex,
          kind: "whitespace",
        });
      }
    }
    return tokens;
  }

  lexicalDiagnostics(): readonly SyntaxDiagnostic[] {
    const diagnostics: SyntaxDiagnostic[] = [];
    for (let lineIndex = 0; lineIndex < this.cache.length; lineIndex += 1) {
      const cached = this.cache[lineIndex];
      if (!cached) continue;
      const lineStart = this.lineStarts[lineIndex];
      for (const diagnostic of cached.view.diagnostics) {
        diagnostics.push({
          ...diagnostic,
          from: lineStart + diagnostic.from,
          to: lineStart + diagnostic.to,
        });
      }
    }
    const finalState = this.cache.at(-1)?.endState ?? this.rootState;
    try {
      const finalize = this.language.finalize;
      if (finalize) {
        for (const diagnostic of finalize.call(this.language, this.source, finalState)) {
          if (!Number.isInteger(diagnostic.from) || !Number.isInteger(diagnostic.to)) {
            throw new RangeError(`Некорректный диапазон диагностики ${diagnostic.from}:${diagnostic.to}`);
          }
          const from = Math.max(0, Math.min(this.source.length, diagnostic.from));
          diagnostics.push({
            ...diagnostic,
            from,
            to: Math.max(from, Math.min(this.source.length, diagnostic.to)),
          });
        }
      }
    } catch (error) {
      diagnostics.push({
        from: this.source.length,
        to: this.source.length,
        severity: "error",
        code: "provider-error",
        message: safeErrorMessage(error, "Ошибка финализации языкового провайдера"),
      });
    }
    return diagnostics;
  }

  applyChange(change: TextChange, options: ChangeOptions = {}): IncrementalUpdate {
    this.assertVersion(options.expectedVersion);
    this.assertChange(change);
    if (
      (change.from === change.to && change.insert.length === 0)
      || this.source.slice(change.from, change.to) === change.insert
    ) {
      const line = this.lineIndexAt(change.from);
      return {
        beforeVersion: this.documentVersion,
        afterVersion: this.documentVersion,
        fromLine: line,
        toLine: line,
        removedLines: 0,
        insertedLines: 0,
        retokenizedLines: 0,
        stabilizedAtLine: line,
      };
    }

    const beforeVersion = this.documentVersion;
    const rawStartLine = this.lineIndexAt(change.from);
    const rawEndLine = this.lineIndexAt(change.to);
    // Include one unchanged neighbour on each side so CR + LF pairing can be
    // recomputed locally even when the edit sits exactly on a line boundary.
    const regionStartLine = Math.max(0, rawStartLine - 1);
    const regionEndLine = Math.min(this.sourceLines.length, rawEndLine + 2);
    const regionStart = this.lineStarts[regionStartLine];
    const regionEnd = regionEndLine < this.sourceLines.length
      ? this.lineStarts[regionEndLine]
      : this.source.length;
    const oldRegionLines = this.sourceLines.slice(regionStartLine, regionEndLine);
    const replacementSource = `${this.source.slice(regionStart, change.from)}${change.insert}${this.source.slice(change.to, regionEnd)}`;
    const replacementLines = splitLines(replacementSource);
    // When a suffix remains, splitLines' terminal empty record belongs to that
    // suffix rather than to the locally replaced window.
    if (regionEndLine < this.sourceLines.length) replacementLines.pop();

    let commonPrefix = 0;
    while (
      commonPrefix < oldRegionLines.length
      && commonPrefix < replacementLines.length
      && sameSourceLine(oldRegionLines[commonPrefix], replacementLines[commonPrefix])
    ) commonPrefix += 1;
    let commonSuffix = 0;
    while (
      commonSuffix < oldRegionLines.length - commonPrefix
      && commonSuffix < replacementLines.length - commonPrefix
      && sameSourceLine(
        oldRegionLines[oldRegionLines.length - commonSuffix - 1],
        replacementLines[replacementLines.length - commonSuffix - 1],
      )
    ) commonSuffix += 1;

    const startLine = regionStartLine + commonPrefix;
    const removedLines = oldRegionLines.length - commonPrefix - commonSuffix;
    const inserted = replacementLines.slice(commonPrefix, replacementLines.length - commonSuffix);
    const nextSource = `${this.source.slice(0, change.from)}${change.insert}${this.source.slice(change.to)}`;
    const characterDelta = change.insert.length - (change.to - change.from);
    const insertedStarts: number[] = [];
    let insertedOffset = this.lineStarts[startLine] ?? this.source.length;
    for (const line of inserted) {
      insertedStarts.push(insertedOffset);
      insertedOffset += line.text.length + line.lineBreak.length;
    }

    replaceArraySegment(this.sourceLines, startLine, removedLines, inserted);
    replaceArraySegment(
      this.cache,
      startLine,
      removedLines,
      new Array<CachedLine<State> | undefined>(inserted.length),
    );
    replaceArraySegment(this.lineStarts, startLine, removedLines, insertedStarts);
    for (let index = startLine + inserted.length; index < this.lineStarts.length; index += 1) {
      this.lineStarts[index] += characterDelta;
    }
    this.source = nextSource;
    const tokenization = this.retokenizeFrom(startLine);
    this.documentVersion += 1;

    return {
      beforeVersion,
      afterVersion: this.documentVersion,
      fromLine: startLine,
      toLine: tokenization.stabilizedAt,
      removedLines,
      insertedLines: inserted.length,
      retokenizedLines: tokenization.retokenized,
      stabilizedAtLine: tokenization.stabilizedAt,
    };
  }

  applyChanges(changes: readonly TextChange[], options: ChangeOptions = {}): IncrementalUpdate {
    this.assertVersion(options.expectedVersion);
    if (!changes.length) {
      return {
        beforeVersion: this.documentVersion,
        afterVersion: this.documentVersion,
        fromLine: 0,
        toLine: 0,
        removedLines: 0,
        insertedLines: 0,
        retokenizedLines: 0,
        stabilizedAtLine: 0,
      };
    }

    const ordered = [...changes].sort((left, right) => left.from - right.from || left.to - right.to);
    for (let index = 0; index < ordered.length; index += 1) {
      this.assertChange(ordered[index]);
      if (index > 0 && ordered[index].from < ordered[index - 1].to) {
        throw new RangeError("Изменения документа пересекаются");
      }
      if (index > 0 && ordered[index].from === ordered[index - 1].from) {
        throw new RangeError("Несколько изменений начинаются в одной позиции");
      }
    }

    const parts: string[] = [];
    let cursor = 0;
    for (const change of ordered) {
      parts.push(this.source.slice(cursor, change.from), change.insert);
      cursor = change.to;
    }
    parts.push(this.source.slice(cursor));
    const nextSource = parts.join("");
    const merged = computeTextChange(this.source, nextSource);
    return merged
      ? this.applyChange(merged, options)
      : this.applyChanges([], options);
  }

  private retokenizeFrom(fromLine: number): { retokenized: number; stabilizedAt: number } {
    let stateKey: (state: State) => string = defaultStateKey;
    try {
      stateKey = this.language.stateKey ?? defaultStateKey;
    } catch {
      // A provider accessor is isolated the same way as a failing key function.
    }
    let state = fromLine === 0
      ? this.rootState
      : this.cache[fromLine - 1]?.endState ?? this.rootState;
    let retokenized = 0;

    for (let index = fromLine; index < this.sourceLines.length; index += 1) {
      let startStateKey: string;
      try {
        startStateKey = stateKey(state);
      } catch {
        startStateKey = `unkeyed:${index}:${nextLineId}`;
      }
      const reusable = this.cache[index];
      if (reusable && this.statesEqual(reusable, state, startStateKey)) {
        return { retokenized, stabilizedAt: index };
      }

      const sourceLine = this.sourceLines[index];
      let endState = state;
      let view: TokenizedLine;
      try {
        const result = this.language.tokenizeLine(sourceLine.text, state);
        // Read every provider-owned property inside the isolation boundary.
        // A getter is allowed to fail just like tokenizeLine itself, but must
        // never leave an already-applied edit with a half-written cache.
        const resultTokens = result.tokens;
        const resultDiagnostics = result.diagnostics;
        endState = result.state;
        view = Object.freeze({
          id: nextLineId++,
          text: sourceLine.text,
          lineBreak: sourceLine.lineBreak,
          tokens: Object.freeze([...normalizeTokens(sourceLine.text, resultTokens)]),
          diagnostics: Object.freeze([...normalizeDiagnostics(sourceLine.text, resultDiagnostics)]),
        });
      } catch (error) {
        endState = state;
        view = Object.freeze({
          id: nextLineId++,
          text: sourceLine.text,
          lineBreak: sourceLine.lineBreak,
          tokens: Object.freeze(sourceLine.text
            ? [Object.freeze({ from: 0, to: sourceLine.text.length, kind: "text" as const })]
            : []),
          diagnostics: Object.freeze([Object.freeze({
            from: 0,
            to: sourceLine.text.length,
            severity: "error" as const,
            code: "provider-error",
            message: safeErrorMessage(error, "Ошибка языкового провайдера"),
          })]),
        });
      }
      this.cache[index] = {
        view,
        startState: state,
        startStateKey,
        endState,
      };
      state = endState;
      retokenized += 1;
    }

    return { retokenized, stabilizedAt: this.sourceLines.length };
  }

  private rebuildLineStarts(): void {
    this.lineStarts = new Array(this.sourceLines.length);
    let offset = 0;
    for (let index = 0; index < this.sourceLines.length; index += 1) {
      this.lineStarts[index] = offset;
      offset += this.sourceLines[index].text.length + this.sourceLines[index].lineBreak.length;
    }
  }

  private lineIndexAt(offset: number): number {
    let low = 0;
    let high = this.lineStarts.length - 1;
    while (low <= high) {
      const middle = (low + high) >>> 1;
      if (this.lineStarts[middle] <= offset) low = middle + 1;
      else high = middle - 1;
    }
    return Math.max(0, high);
  }

  private assertOffset(offset: number): void {
    if (!Number.isInteger(offset) || offset < 0 || offset > this.source.length) {
      throw new RangeError(`Смещение ${offset} находится вне документа`);
    }
  }

  private assertChange(change: TextChange): void {
    this.assertOffset(change.from);
    this.assertOffset(change.to);
    if (change.from > change.to) throw new RangeError("Начало изменения находится после конца");
  }

  private assertVersion(expectedVersion: number | undefined): void {
    if (expectedVersion !== undefined && expectedVersion !== this.documentVersion) {
      throw new Error(`Устаревшая версия документа: ${expectedVersion}; текущая: ${this.documentVersion}`);
    }
  }

  private statesEqual(cached: CachedLine<State>, state: State, stateKey: string): boolean {
    try {
      const statesEqual = this.language.statesEqual;
      if (statesEqual) return statesEqual.call(this.language, cached.startState, state);
      if (this.language.stateKey) return cached.startStateKey === stateKey;
    } catch {
      return false;
    }
    return Object.is(cached.startState, state);
  }
}
