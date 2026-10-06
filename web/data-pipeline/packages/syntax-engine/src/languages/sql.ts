import { createCodeLanguage } from "./code.js";

const words = (source: string): readonly string[] => source.trim().split(/\s+/u);

export const sqlLanguage = createCodeLanguage({
  id: "sql",
  name: "SQL",
  aliases: ["ansi-sql"],
  extensions: ["sql", "ddl", "dml"],
  mimeTypes: ["application/sql", "text/x-sql"],
  caseInsensitive: true,
  keywords: words(`
    add all alter analyze and any as asc authorization backup begin between both by cascade case cast check
    collate column commit constraint create cross current current_date current_time current_timestamp database
    default delete desc distinct do drop else end escape except exists explain false fetch first for foreign
    from full function grant group having if in index inner insert intersect into is join lateral left like limit
    merge natural no not null nulls offset on only or order outer over partition primary procedure references
    returning revoke right rollback row rows schema select set some table temporary then to transaction trigger
    true truncate union unique update using values view when where window with
  `),
  types: words(`
    bigint binary bit blob boolean char character clob date decimal double float int integer interval json
    jsonb money numeric real serial smallint text time timestamp uuid varchar varying xml
  `),
  booleans: ["true", "false"],
  nulls: ["null", "unknown"],
  lineComments: ["--"],
  lineStartComments: ["#"],
  blockComment: { open: "/*", close: "*/", nested: true },
  operators: [
    "->>", "#>>", "::", "||", "&&", "<=", ">=", "<>", "!=", ":=", "->", "#>", "@>", "<@", "?&",
    "?|", "!~*", "!~", "~*", "+", "-", "*", "/", "%", "=", "<", ">", "~", "&", "|", "^", ":",
  ],
  punctuation: "()[],.;{}",
  strings: [
    { open: /^\$[A-Za-z_][A-Za-z_0-9]*\$|^\$\$/, close: (opening) => opening, multiline: true, escape: "none" },
    { open: "'", close: "'", multiline: true, escape: "double" },
    { open: '"', close: '"', multiline: true, escape: "double", kind: "identifier" },
    { open: "`", close: "`", multiline: true, escape: "double", kind: "identifier" },
  ],
});
