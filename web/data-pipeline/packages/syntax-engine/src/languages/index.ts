import type { LanguageDefinition } from "../types.js";
import {
  csharpLanguage,
  javaLanguage,
  javascriptLanguage,
  rustLanguage,
  typescriptLanguage,
} from "./code-profiles.js";
import { iniLanguage } from "./ini.js";
import { jsonLanguage } from "./json.js";
import { pythonLanguage } from "./python.js";
import { sqlLanguage } from "./sql.js";
import { xmlLanguage } from "./xml.js";
import { yamlLanguage } from "./yaml.js";

export { createCodeLanguage } from "./code.js";
export type {
  CodeLanguageConfig,
  CodeLexerState,
  StringEscape,
  StringRule,
} from "./code.js";

export const BUILTIN_LANGUAGES: readonly LanguageDefinition<any>[] = Object.freeze([
  csharpLanguage,
  rustLanguage,
  javascriptLanguage,
  typescriptLanguage,
  pythonLanguage,
  sqlLanguage,
  jsonLanguage,
  xmlLanguage,
  yamlLanguage,
  javaLanguage,
  iniLanguage,
]);

export {
  csharpLanguage,
  iniLanguage,
  javaLanguage,
  javascriptLanguage,
  jsonLanguage,
  pythonLanguage,
  rustLanguage,
  sqlLanguage,
  typescriptLanguage,
  xmlLanguage,
  yamlLanguage,
};
