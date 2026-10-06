import { SyntaxDocument } from "./document.js";
import type {
  FormatOptions,
  FormatResult,
  LanguageDefinition,
  LanguageCapabilities,
  LanguageServices,
  LanguageServiceOptions,
  SyntaxDiagnostic,
} from "./types.js";

function normalizeName(value: string): string {
  return value.trim().toLowerCase().replace(/^\./, "");
}

export class LanguageRegistry {
  private readonly languages = new Map<string, LanguageDefinition<any>>();
  private readonly aliases = new Map<string, string>();
  private readonly extensions = new Map<string, string>();
  private readonly mimeTypes = new Map<string, string>();
  private readonly services = new Map<string, LanguageServices>();

  register<State>(language: LanguageDefinition<State>, options: { replace?: boolean } = {}): this {
    const id = normalizeName(language.id);
    if (!id) throw new Error("Язык должен иметь непустой id");
    if (this.languages.has(id) && !options.replace) {
      throw new Error(`Язык «${language.id}» уже зарегистрирован`);
    }

    const aliases = [id, ...(language.aliases ?? []).map(normalizeName)];
    const extensions = (language.extensions ?? []).map(normalizeName);
    const mimeTypes = (language.mimeTypes ?? []).map(normalizeName);
    this.assertIndexesAvailable(this.aliases, aliases, id);
    this.assertIndexesAvailable(this.extensions, extensions, id);
    this.assertIndexesAvailable(this.mimeTypes, mimeTypes, id);

    if (options.replace) this.removeIndexesFor(id);
    this.languages.set(id, language);
    this.aliases.set(id, id);
    for (const alias of language.aliases ?? []) this.index(this.aliases, alias, id);
    for (const extension of language.extensions ?? []) {
      this.index(this.extensions, extension, id);
    }
    for (const mimeType of language.mimeTypes ?? []) {
      this.index(this.mimeTypes, mimeType.toLowerCase(), id);
    }
    return this;
  }

  registerServices(languageId: string, services: LanguageServices): this {
    const language = this.require(languageId);
    const id = normalizeName(language.id);
    this.services.set(id, { ...this.services.get(id), ...services });
    return this;
  }

  get(idOrAlias: string): LanguageDefinition<any> | undefined {
    const id = this.aliases.get(normalizeName(idOrAlias)) ?? normalizeName(idOrAlias);
    return this.languages.get(id);
  }

  require(idOrAlias: string): LanguageDefinition<any> {
    const language = this.get(idOrAlias);
    if (!language) throw new Error(`Язык «${idOrAlias}» не зарегистрирован`);
    return language;
  }

  list(): readonly LanguageDefinition<any>[] {
    return [...this.languages.values()];
  }

  capabilities(idOrAlias: string): LanguageCapabilities {
    const language = this.require(idOrAlias);
    const services = this.services.get(normalizeName(language.id));
    const validate = services?.validate ?? language.validate;
    return Object.freeze({
      validation: validate
        ? services?.validationLevel ?? language.validationLevel ?? "structural"
        : "lexical",
      formatting: Boolean(services?.format ?? language.format),
    });
  }

  detect(fileNameOrExtension: string, mimeType?: string): LanguageDefinition<any> | undefined {
    if (mimeType) {
      const mimeId = this.mimeTypes.get(mimeType.toLowerCase().split(";", 1)[0].trim());
      if (mimeId) return this.languages.get(mimeId);
    }
    const normalized = normalizeName(fileNameOrExtension);
    const direct = this.get(normalized);
    if (direct) return direct;
    const lastDot = normalized.lastIndexOf(".");
    const extension = lastDot >= 0 ? normalized.slice(lastDot + 1) : normalized;
    const id = this.extensions.get(extension);
    return id ? this.languages.get(id) : undefined;
  }

  createDocument(source: string, idOrAlias: string): SyntaxDocument<unknown> {
    return new SyntaxDocument(
      source,
      this.require(idOrAlias) as unknown as LanguageDefinition<unknown>,
    );
  }

  async validate(
    source: string,
    idOrAlias: string,
    options?: LanguageServiceOptions,
  ): Promise<readonly SyntaxDiagnostic[]> {
    const language = this.require(idOrAlias);
    const service = this.services.get(normalizeName(language.id))?.validate ?? language.validate;
    if (service) return service(source, options);
    return new SyntaxDocument(source, language).lexicalDiagnostics();
  }

  async format(
    source: string,
    idOrAlias: string,
    options?: FormatOptions & LanguageServiceOptions,
  ): Promise<FormatResult> {
    const language = this.require(idOrAlias);
    const service = this.services.get(normalizeName(language.id))?.format ?? language.format;
    if (!service) {
      return {
        ok: false,
        diagnostics: [{
          from: 0,
          to: 0,
          severity: "info",
          code: "formatter-unavailable",
          message: `Для языка ${language.name} форматтер ещё не подключён`,
        }],
      };
    }
    return service(source, options);
  }

  private index(
    index: Map<string, string>,
    rawKey: string,
    id: string,
  ): void {
    const key = normalizeName(rawKey);
    const existing = index.get(key);
    if (existing && existing !== id) {
      throw new Error(`Имя «${rawKey}» уже принадлежит языку «${existing}»`);
    }
    index.set(key, id);
  }

  private assertIndexesAvailable(index: Map<string, string>, keys: readonly string[], id: string): void {
    for (const key of keys) {
      if (!key) throw new Error("Пустое имя, расширение или MIME type недопустимы");
      const existing = index.get(key);
      if (existing && existing !== id) {
        throw new Error(`Имя «${key}» уже принадлежит языку «${existing}»`);
      }
    }
  }

  private removeIndexesFor(id: string): void {
    for (const index of [this.aliases, this.extensions, this.mimeTypes]) {
      for (const [key, value] of index) if (value === id) index.delete(key);
    }
  }
}
