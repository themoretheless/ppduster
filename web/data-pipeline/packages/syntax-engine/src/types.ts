export type TokenKind =
  | "whitespace"
  | "comment"
  | "keyword"
  | "type"
  | "identifier"
  | "property"
  | "function"
  | "number"
  | "string"
  | "escape"
  | "regexp"
  | "boolean"
  | "null"
  | "operator"
  | "punctuation"
  | "tag"
  | "attribute"
  | "decorator"
  | "macro"
  | "directive"
  | "section"
  | "text"
  | "invalid";

export interface LineToken {
  /** UTF-16 offsets relative to the start of the line. */
  readonly from: number;
  readonly to: number;
  readonly kind: TokenKind;
  readonly modifiers?: readonly string[];
}

export interface SyntaxToken extends LineToken {
  /** UTF-16 offsets relative to the start of the document. */
  readonly from: number;
  readonly to: number;
  readonly line: number;
}

export type DiagnosticSeverity = "info" | "warning" | "error";

export interface SyntaxDiagnostic {
  readonly from: number;
  readonly to: number;
  readonly message: string;
  readonly severity: DiagnosticSeverity;
  readonly code?: string;
}

export interface LineDiagnostic extends Omit<SyntaxDiagnostic, "from" | "to"> {
  /** UTF-16 offsets relative to the start of the line. */
  readonly from: number;
  readonly to: number;
}

export interface LineTokenization<State> {
  readonly tokens: readonly LineToken[];
  readonly state: State;
  readonly diagnostics?: readonly LineDiagnostic[];
}

export interface FormatOptions {
  readonly indent?: number | "tab";
  readonly finalNewline?: boolean;
}

export type FormatResult =
  | { readonly ok: true; readonly text: string }
  | { readonly ok: false; readonly diagnostics: readonly SyntaxDiagnostic[] };

export type MaybePromise<Value> = Value | Promise<Value>;

export interface LanguageServiceOptions {
  readonly signal?: AbortSignal;
}

export type ValidationLevel = "lexical" | "structural";

export interface LanguageCapabilities {
  /** Structural validation parses the whole document; lexical validation is tokenizer-based. */
  readonly validation: ValidationLevel;
  readonly formatting: boolean;
}

export interface LanguageServices {
  readonly validationLevel?: ValidationLevel;
  readonly validate?: (
    source: string,
    options?: LanguageServiceOptions,
  ) => MaybePromise<readonly SyntaxDiagnostic[]>;
  readonly format?: (
    source: string,
    options?: FormatOptions & LanguageServiceOptions,
  ) => MaybePromise<FormatResult>;
}

/**
 * A language definition is deliberately line-oriented. The returned state is
 * fed into the next line, which makes multi-line strings and comments possible
 * while allowing the document engine to stop re-tokenizing once state
 * converges after an edit.
 */
export interface LanguageDefinition<State = unknown> extends LanguageServices {
  readonly id: string;
  readonly name: string;
  readonly aliases?: readonly string[];
  readonly extensions?: readonly string[];
  readonly mimeTypes?: readonly string[];
  readonly initialState: () => State;
  readonly tokenizeLine: (line: string, state: Readonly<State>) => LineTokenization<State>;
  /** Exact semantic comparison; preferred for persistent/custom state objects. */
  readonly statesEqual?: (left: Readonly<State>, right: Readonly<State>) => boolean;
  /** Must return the same value for states with identical lexical meaning. */
  readonly stateKey?: (state: Readonly<State>) => string;
  /** Reports errors that can only be known when the end of input is reached. */
  readonly finalize?: (source: string, state: Readonly<State>) => readonly SyntaxDiagnostic[];
}

export interface TokenizedLine {
  /** Stable while this line's text and incoming lexical state are unchanged. */
  readonly id: number;
  readonly text: string;
  readonly lineBreak: string;
  readonly tokens: readonly LineToken[];
  readonly diagnostics: readonly LineDiagnostic[];
}

export interface TextChange {
  /** UTF-16 document offsets, matching textarea selection offsets. */
  readonly from: number;
  readonly to: number;
  readonly insert: string;
}

export interface IncrementalUpdate {
  readonly beforeVersion: number;
  readonly afterVersion: number;
  readonly fromLine: number;
  readonly toLine: number;
  readonly removedLines: number;
  readonly insertedLines: number;
  readonly retokenizedLines: number;
  readonly stabilizedAtLine: number;
}

export interface ChangeOptions {
  readonly expectedVersion?: number;
}

export interface TextPosition {
  readonly line: number;
  readonly column: number;
}
