# @peregon/syntax-engine

Собственный UI-независимый движок токенизации для браузера и Node.js. Он
работает с UTF-16 offsets (как `textarea`), хранит токены относительно строк и
после изменения пересчитывает только затронутые строки и участок, на который
распространилось лексическое состояние.

## Установка

```bash
npm install @peregon/syntax-engine
```

Пакет поставляет ESM JavaScript и декларации TypeScript. Он не зависит от Vue,
редактора, DOM или других runtime-библиотек. Для минимальной сборки без
автоматической регистрации встроенных языков можно импортировать ядро из
`@peregon/syntax-engine/core`, а отдельные профили — из
`@peregon/syntax-engine/languages/json` и других языковых subpath exports.

## Встроенные языки

| Язык | ID | Основные конструкции | Встроенная проверка | Форматтер |
| --- | --- | --- | --- | --- |
| C# | `csharp` | директивы, verbatim/interpolated/raw strings, комментарии | лексическая | hook |
| Rust | `rust` | вложенные комментарии, raw strings, lifetime, macro, raw identifier | лексическая | hook |
| JavaScript | `javascript` | regex literals, template strings, комментарии | лексическая | hook |
| TypeScript | `typescript` | JavaScript + TS keywords и типы | лексическая | hook |
| Python | `python` | decorators, prefixed/triple strings, keywords | лексическая | hook |
| SQL | `sql` | регистронезависимые keywords, identifiers, dollar strings | лексическая | hook |
| JSON | `json` | структурные состояния, точные числа | структурная | встроен |
| XML | `xml` | tags, attributes, entities, DTD, CDATA, PI | структурная | hook |
| YAML | `yaml` | keys, YAML 1.2 Core scalars, anchors, tags, block scalars | лексическая | hook |
| Java | `java` | annotations, text blocks, comments, keywords | лексическая | hook |
| INI | `ini` | sections, properties, typed values, comments, continuations | лексическая | hook |

Лексическая диагностика доступна для всех профилей. Строгие JSON-проверка и
lossless-форматтер не преобразуют числа через JavaScript `number`. XML-проверка
контролирует well-formedness основной структуры и поддерживаемого подмножества
внутренних entities/DTD, но не является validating XML processor, не заменяет
XSD, не загружает внешние DTD и не раскрывает parameter entities. Для
компиляторов, language server,
Prettier-подобных форматтеров или WASM-парсера есть асинхронные service hooks.
Различение regexp и деления в JavaScript/TypeScript остаётся лексической
эвристикой; для точной грамматической проверки подключается parser/compiler hook.

## Использование

```ts
import { syntaxEngine } from "@peregon/syntax-engine";

const document = syntaxEngine.createDocument(
  "interface User { name: string }",
  "typescript",
);

const update = document.applyChange(
  { from: 10, to: 14, insert: "Account" },
  { expectedVersion: document.version },
);

// TokenizedLine сохраняет identity, пока текст и входное состояние не менялись.
for (const line of document.lines.slice(update.fromLine, update.toLine)) {
  renderLine(line);
}

const diagnostics = await syntaxEngine.validate(document.text, "typescript");
const formatted = await syntaxEngine.format(document.text, "json", { indent: 2 });

// Можно заранее построить UI по реально доступным возможностям.
syntaxEngine.capabilities("typescript");
// { validation: "lexical", formatting: false }
```

Минимальный registry с выбранными языками:

```ts
import { LanguageRegistry } from "@peregon/syntax-engine/core";
import { jsonLanguage } from "@peregon/syntax-engine/languages/json";

const registry = new LanguageRegistry().register(jsonLanguage);
```

`tokens()` строит плоский список с абсолютными offsets и удобен для экспорта или
тестов. В горячем editor path нужно использовать `lines`: неизменившиеся
`TokenizedLine` и их токены переиспользуются без новых объектов и перерисовки.

## Новый язык

```ts
import { LanguageRegistry } from "@peregon/syntax-engine";

const registry = new LanguageRegistry();

registry.register({
  id: "demo",
  name: "Demo",
  aliases: ["d"],
  extensions: ["demo"],
  initialState: () => "root",
  stateKey: (state) => state,
  tokenizeLine(line, state) {
    return {
      tokens: line ? [{ from: 0, to: line.length, kind: "text" }] : [],
      state,
    };
  },
  finalize(source, state) {
    return [];
  },
});

registry.registerServices("demo", {
  async validate(source, { signal } = {}) {
    return languageServer.validate(source, signal);
  },
  async format(source, options) {
    return { ok: true, text: await formatter.format(source, options) };
  },
});
```

Состояние провайдера должно быть неизменяемым. Для stateful-профиля следует
задать точный `statesEqual` или `stateKey`, возвращающий одинаковое значение для
семантически одинаковых состояний. Без них движок использует безопасное
сравнение identity: корректность сохраняется, но пересчёт может пройти дальше
необходимого.

Токены одной строки должны быть отсортированы, не пересекаться и лежать внутри
`[0, line.length]`. Нарушение контракта изолируется диагностикой
`provider-error`, не ломая весь документ.

## Производительность

`applyChange()` пересобирает только маленькое окно строк вокруг правки, затем
продолжает токенизацию до совпадения входного состояния с кэшем. Суффиксные
строки и объекты токенов сохраняют identity. `npm run bench:syntax` измеряет
холодный запуск и локальные правки на документе 1 MiB; на машине разработки
локальный edit p95 для встроенных профилей находится примерно в диапазоне
0.10–0.53 ms. Значения зависят от железа и содержимого документа.
