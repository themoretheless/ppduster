import { createCodeLanguage, type CodeLanguageConfig } from "./code.js";

const words = (source: string): readonly string[] => source.trim().split(/\s+/u);

const COMMON_C_TYPES = words(`
  bool byte char decimal double dynamic float int long object sbyte short string uint ulong ushort void
`);

const C_SHARP: CodeLanguageConfig = {
  id: "csharp",
  name: "C#",
  aliases: ["cs", "c#", "dotnet"],
  extensions: ["cs", "csx"],
  mimeTypes: ["text/x-csharp"],
  keywords: words(`
    abstract as base break case catch checked class const continue default delegate do else enum event
    explicit extern finally fixed for foreach goto if implicit in interface internal is lock namespace new
    operator out override params private protected public readonly record ref return sealed sizeof stackalloc
    static struct switch this throw try typeof unchecked unsafe using virtual volatile while add alias and
    ascending async await by descending equals file from get global group init into join let managed nameof
    nint not notnull nuint on or orderby partial remove required scoped select set unmanaged value var when
    where with yield
  `),
  types: COMMON_C_TYPES,
  lineComments: ["//"],
  blockComment: { open: "/*", close: "*/" },
  directiveMarker: "#",
  rawIdentifierPrefix: "@",
  numberPattern: /^(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*|0[bB][01](?:_?[01])*|(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?)(?:[fFdDmM]|[uU](?:[lL])?|[lL](?:[uU])?)?/,
  strings: [
    {
      open: /^\$*"{3,}/,
      close: (opening) => '"'.repeat(opening.match(/"+$/u)?.[0].length ?? 3),
      multiline: true,
      escape: "none",
    },
    { open: "$@\"", close: '"', multiline: true, escape: "double" },
    { open: "@$\"", close: '"', multiline: true, escape: "double" },
    { open: "@\"", close: '"', multiline: true, escape: "double" },
    { open: "$\"", close: '"', escape: "backslash" },
    { open: '"', escape: "backslash" },
    { open: "'", escape: "backslash" },
  ],
};

const RUST: CodeLanguageConfig = {
  id: "rust",
  name: "Rust",
  aliases: ["rs"],
  extensions: ["rs"],
  mimeTypes: ["text/rust", "text/x-rust"],
  keywords: words(`
    as async await break const continue crate dyn else enum extern false fn for if impl in let loop match
    mod move mut pub ref return self Self static struct super trait true type union unsafe use where while
    abstract become box do final macro override priv typeof unsized virtual yield try
  `),
  types: words(`
    bool char str i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize f32 f64 String Vec Option Result Box
  `),
  lineComments: ["//"],
  blockComment: { open: "/*", close: "*/", nested: true },
  macroSuffix: "!",
  lifetimeIdentifiers: true,
  rawIdentifierPrefix: "r#",
  numberPattern: /^(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*|0[bB][01](?:_?[01])*|0[oO][0-7](?:_?[0-7])*|(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?)(?:(?:[iu](?:8|16|32|64|128|size))|f(?:32|64))?/,
  strings: [
    {
      open: /^(?:br|r)(#{0,255})"/,
      close: (_opening, match) => `"${match?.[1] ?? ""}`,
      multiline: true,
      escape: "none",
    },
    { open: "b\"", close: '"', multiline: true, escape: "backslash" },
    { open: "b'", close: "'", escape: "backslash" },
    { open: '"', multiline: true, escape: "backslash" },
    { open: "'", escape: "backslash" },
  ],
};

const JAVASCRIPT_KEYWORDS = words(`
  async await break case catch class const continue debugger default delete do else export extends finally
  for from function get if import in instanceof let new of return set static super switch this throw try
  typeof var void while with yield
`);

const JAVASCRIPT: CodeLanguageConfig = {
  id: "javascript",
  name: "JavaScript",
  aliases: ["js", "ecmascript", "mjs", "cjs"],
  extensions: ["js", "mjs", "cjs"],
  mimeTypes: ["text/javascript", "application/javascript"],
  keywords: JAVASCRIPT_KEYWORDS,
  types: words(`Array ArrayBuffer BigInt Boolean Date Error Function Map Number Object Promise RegExp Set String Symbol WeakMap WeakSet`),
  booleans: ["true", "false"],
  nulls: ["null", "undefined"],
  lineComments: ["//"],
  blockComment: { open: "/*", close: "*/" },
  regexLiterals: true,
  numberPattern: /^(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*n?|0[bB][01](?:_?[01])*n?|0[oO][0-7](?:_?[0-7])*n?|\d(?:_?\d)*n|(?:(?:\d(?:_?\d)*)?\.\d(?:_?\d)*|\d(?:_?\d)*\.)(?:[eE][+-]?\d(?:_?\d)*)?|\d(?:_?\d)*(?:[eE][+-]?\d(?:_?\d)*)?)/,
  extraIdentifierStart: "$",
  extraIdentifierPart: "$",
  strings: [
    { open: "`", multiline: true, escape: "backslash" },
    { open: '"', lineContinuation: true, escape: "backslash" },
    { open: "'", lineContinuation: true, escape: "backslash" },
  ],
};

const TYPESCRIPT: CodeLanguageConfig = {
  ...JAVASCRIPT,
  id: "typescript",
  name: "TypeScript",
  aliases: ["ts"],
  extensions: ["ts", "mts", "cts"],
  mimeTypes: ["text/typescript", "application/typescript"],
  keywords: [
    ...JAVASCRIPT_KEYWORDS,
    ...words(`
      abstract any as asserts bigint boolean constructor declare enum global implements infer interface
      intrinsic is keyof module namespace never number object override private protected public readonly
      require satisfies string symbol type unique unknown
    `),
  ],
  types: words(`
    any bigint boolean never number object string symbol unknown void Array Date Map Promise Record Set
    Partial Required Readonly Pick Omit Exclude Extract NonNullable Parameters ReturnType
  `),
};

const JAVA: CodeLanguageConfig = {
  id: "java",
  name: "Java",
  aliases: ["jdk"],
  extensions: ["java"],
  mimeTypes: ["text/x-java-source", "text/x-java"],
  keywords: words(`
    abstract assert break case catch class const continue default do else enum exports extends final finally
    for goto if implements import instanceof interface module native new non sealed open opens package permits
    private protected provides public record requires return sealed static strictfp super switch synchronized
    this throw throws to transient transitive try uses var volatile while with yield
  `),
  types: words(`boolean byte char double float int long short void String Object Integer Long Double Float Boolean Character`),
  lineComments: ["//"],
  blockComment: { open: "/*", close: "*/" },
  decorators: true,
  extraIdentifierStart: "$",
  extraIdentifierPart: "$",
  numberPattern: /^(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*|0[bB][01](?:_?[01])*|(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?)[lLfFdD]?/,
  strings: [
    { open: '"""', close: '"""', multiline: true, escape: "backslash" },
    { open: '"', escape: "backslash" },
    { open: "'", escape: "backslash" },
  ],
};

export const csharpLanguage = createCodeLanguage(C_SHARP);
export const rustLanguage = createCodeLanguage(RUST);
export const javascriptLanguage = createCodeLanguage(JAVASCRIPT);
export const typescriptLanguage = createCodeLanguage(TYPESCRIPT);
export const javaLanguage = createCodeLanguage(JAVA);
