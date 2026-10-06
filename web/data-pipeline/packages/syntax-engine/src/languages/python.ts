import { createCodeLanguage } from "./code.js";

const words = (source: string): readonly string[] => source.trim().split(/\s+/u);

export const pythonLanguage = createCodeLanguage({
  id: "python",
  name: "Python",
  aliases: ["py", "python3"],
  extensions: ["py", "pyi", "pyw"],
  mimeTypes: ["text/x-python", "application/x-python-code"],
  keywords: words(`
    and as assert async await break case class continue def del elif else except finally for from global if
    import in is lambda match nonlocal not or pass raise return try while with yield
  `),
  types: words(`
    bool bytearray bytes complex dict float frozenset int list memoryview object range set slice str tuple type
  `),
  builtins: words(`
    abs all any ascii bin breakpoint callable chr classmethod compile delattr dir divmod enumerate eval exec
    filter format getattr globals hasattr hash help hex id input isinstance issubclass iter len locals map max
    min next oct open ord pow print property repr reversed round setattr sorted staticmethod sum super vars zip
  `),
  booleans: ["True", "False"],
  nulls: ["None", "NotImplemented", "Ellipsis"],
  lineComments: ["#"],
  decorators: true,
  decoratorsAtLineStart: true,
  operators: [
    "**=", "//=", ">>=", "<<=", ":=", "==", "!=", "<=", ">=", "->", "**", "//", "<<", ">>",
    "+=", "-=", "*=", "/=", "%=", "@=", "&=", "|=", "^=", "+", "-", "*", "/", "%", "@", "&",
    "|", "^", "~", ":", "=", "<", ">",
  ],
  punctuation: "{}[]();,.",
  numberPattern: /^(?:(?:0[xX][\da-fA-F](?:_?[\da-fA-F])*)|(?:0[oO][0-7](?:_?[0-7])*)|(?:0[bB][01](?:_?[01])*)|(?:(?:\d(?:_?\d)*)?(?:\.\d(?:_?\d)*)|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?)(?:[eE][+-]?\d(?:_?\d)*)?)[jJ]?/,
  strings: [
    { open: /^(?:[rRuUbBfF]{0,2})"""/, close: '"""', multiline: true, escape: "backslash" },
    { open: /^(?:[rRuUbBfF]{0,2})'''/, close: "'''", multiline: true, escape: "backslash" },
    { open: /^(?:[rRuUbBfF]{0,2})"/, close: '"', lineContinuation: true, escape: "backslash" },
    { open: /^(?:[rRuUbBfF]{0,2})'/, close: "'", lineContinuation: true, escape: "backslash" },
  ],
});
