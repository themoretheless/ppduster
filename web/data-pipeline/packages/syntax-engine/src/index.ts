import { LanguageRegistry } from "./registry.js";
import { BUILTIN_LANGUAGES } from "./languages/index.js";

export { SyntaxDocument, computeTextChange } from "./document.js";
export { LanguageRegistry } from "./registry.js";
export * from "./types.js";
export * from "./languages/index.js";

export function createSyntaxEngine(): LanguageRegistry {
  const registry = new LanguageRegistry();
  for (const language of BUILTIN_LANGUAGES) registry.register(language);
  return registry;
}

export const syntaxEngine = createSyntaxEngine();
