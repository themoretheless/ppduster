<script setup lang="ts">
import { computed, ref, shallowRef, watch } from "vue";
import {
  computeTextChange,
  syntaxEngine,
  type SyntaxDocument,
  type TokenKind,
  type TokenizedLine,
} from "@peregon/syntax-engine";
import {
  canBuildSyntaxSnapshot,
  dominantEol,
  exceedsHighlightTokenLimit,
  withEditorEol,
  type EditorEol,
} from "./json-code-editor-policy.ts";

const props = defineProps<{
  modelValue?: string;
  label: string;
  readonly?: boolean;
  language?: string;
  highlightSyntax?: boolean;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string];
}>();

const textarea = ref<HTMLTextAreaElement | null>(null);
const highlight = ref<HTMLElement | null>(null);
const readonlyContent = ref<HTMLElement | null>(null);
const gutter = ref<HTMLElement | null>(null);
const copied = ref(false);
let copiedTimer: ReturnType<typeof setTimeout> | undefined;
const requestedLanguage = () => props.language ?? "json";
const languageProblem = (language = requestedLanguage()) => syntaxEngine.get(language)
  ? ""
  : `Язык «${language}» не зарегистрирован; используется JSON`;
let activeLanguage = syntaxEngine.get(requestedLanguage())?.id ?? "json";
const languageLabel = computed(() => syntaxEngine.get(requestedLanguage())?.name ?? "JSON");
const formatError = ref(languageProblem());
let currentValue = props.modelValue ?? "";
let eolConvention: EditorEol = dominantEol(currentValue);
let contentRevision = 1;
let syntaxDocument: SyntaxDocument<unknown> | null = null;
const renderedValue = shallowRef(currentValue);
const textareaValue = computed(() => withEditorEol(renderedValue.value, "\n"));
const lineNumbers = computed(() => Array.from(
  { length: Math.max(1, textareaValue.value.split("\n").length) },
  (_, index) => index + 1,
));
const lines = shallowRef<readonly TokenizedLine[]>([]);
const renderSyntax = ref(false);
const largeFileMode = ref(false);
const toolbarLabel = computed(() => formatError.value
  || (largeFileMode.value ? `${languageLabel.value} · большой файл, без подсветки` : languageLabel.value));

function refreshSyntax(forceReplace = false) {
  const highlightEnabled = props.highlightSyntax !== false;
  if (!canBuildSyntaxSnapshot(currentValue, highlightEnabled)) {
    syntaxDocument = null;
    lines.value = [];
    renderSyntax.value = false;
    largeFileMode.value = highlightEnabled && currentValue.length > 0;
    return;
  }

  if (forceReplace || !syntaxDocument || syntaxDocument.language.id !== activeLanguage) {
    syntaxDocument = syntaxEngine.createDocument(currentValue, activeLanguage);
  } else {
    const change = computeTextChange(syntaxDocument.text, currentValue);
    if (change) syntaxDocument.applyChange(change);
  }

  const snapshot = syntaxDocument.lines;
  if (exceedsHighlightTokenLimit(snapshot)) {
    lines.value = [];
    renderSyntax.value = false;
    largeFileMode.value = true;
    return;
  }
  lines.value = snapshot;
  renderSyntax.value = true;
  largeFileMode.value = false;
}

function replaceDocument(value: string, language = props.language ?? "json") {
  activeLanguage = syntaxEngine.get(language)?.id ?? "json";
  currentValue = value;
  renderedValue.value = value;
  contentRevision += 1;
  syntaxDocument = null;
  refreshSyntax(true);
  formatError.value = languageProblem(language);
}

function applyValue(value: string) {
  if (currentValue === value) return;
  currentValue = value;
  renderedValue.value = value;
  contentRevision += 1;
  refreshSyntax();
  formatError.value = languageProblem();
}

refreshSyntax(true);

watch(() => props.modelValue ?? "", (value) => {
  eolConvention = dominantEol(value, eolConvention);
  applyValue(value);
});
watch(() => props.language ?? "json", (language) => replaceDocument(props.modelValue ?? "", language));
watch(() => props.highlightSyntax !== false, () => {
  syntaxDocument = null;
  refreshSyntax(true);
});

function update(event: Event) {
  const value = withEditorEol((event.target as HTMLTextAreaElement).value, eolConvention);
  applyValue(value);
  emit("update:modelValue", value);
}

function syncScroll() {
  const scroller = props.readonly ? readonlyContent.value : textarea.value;
  if (!scroller) return;
  if (highlight.value) {
    highlight.value.scrollTop = scroller.scrollTop;
    highlight.value.scrollLeft = scroller.scrollLeft;
  }
  if (gutter.value) gutter.value.scrollTop = scroller.scrollTop;
}

async function copyAll() {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(currentValue);
  } else {
    const fallback = document.createElement("textarea");
    fallback.value = currentValue;
    fallback.style.position = "fixed";
    fallback.style.opacity = "0";
    document.body.append(fallback);
    fallback.select();
    document.execCommand("copy");
    fallback.remove();
  }
  copied.value = true;
  if (copiedTimer) clearTimeout(copiedTimer);
  copiedTimer = setTimeout(() => {
    copied.value = false;
  }, 1400);
}

const invalidTokens = new WeakMap<TokenizedLine, ReadonlySet<number>>();

function invalidTokenIndexes(line: TokenizedLine): ReadonlySet<number> {
  const cached = invalidTokens.get(line);
  if (cached) return cached;
  const errors = line.diagnostics
    .filter((item) => item.severity === "error" && item.to > item.from)
    .slice()
    .sort((left, right) => left.from - right.from || left.to - right.to);
  const indexes = new Set<number>();
  let diagnosticIndex = 0;
  for (let tokenIndex = 0; tokenIndex < line.tokens.length; tokenIndex += 1) {
    const token = line.tokens[tokenIndex];
    while (errors[diagnosticIndex]?.to <= token.from) diagnosticIndex += 1;
    const diagnostic = errors[diagnosticIndex];
    if (diagnostic && diagnostic.from < token.to && diagnostic.to > token.from) indexes.add(tokenIndex);
  }
  invalidTokens.set(line, indexes);
  return indexes;
}

function tokenClass(kind: TokenKind, line: TokenizedLine, tokenIndex: number) {
  return [
    `syntax-token-${kind}`,
    { "syntax-token-invalid": invalidTokenIndexes(line).has(tokenIndex) },
  ];
}

async function format() {
  const requestRevision = contentRevision;
  const requestLanguage = activeLanguage;
  const requestValue = currentValue;
  let result;
  try {
    result = await syntaxEngine.format(requestValue, requestLanguage, { indent: 2 });
  } catch (error) {
    if (contentRevision === requestRevision && activeLanguage === requestLanguage) {
      formatError.value = error instanceof Error ? error.message : "Не удалось форматировать документ";
    }
    return;
  }
  if (contentRevision !== requestRevision || activeLanguage !== requestLanguage) return;
  if (!result.ok) {
    formatError.value = result.diagnostics[0]?.message ?? "Не удалось форматировать документ";
    return;
  }
  const formatted = withEditorEol(result.text, eolConvention);
  applyValue(formatted);
  emit("update:modelValue", formatted);
}

function handleKeydown(event: KeyboardEvent) {
  if (event.altKey && event.shiftKey && event.key.toLowerCase() === "f") {
    event.preventDefault();
    format();
  }
}
</script>

<template>
  <div class="json-code-editor has-toolbar" :class="{ 'is-readonly': readonly }">
    <div class="json-editor-toolbar">
      <span :class="{ 'is-error': formatError }">{{ toolbarLabel }}</span>
      <div class="json-editor-actions">
        <button
          type="button"
          title="Копировать весь текст"
          @pointerdown.stop
          @click.stop="copyAll"
        >{{ copied ? "Скопировано" : "Копировать всё" }}</button>
        <button
          v-if="!readonly"
          type="button"
          title="Форматировать (Shift+Alt+F)"
          @pointerdown.stop
          @click.stop="format"
        >Форматировать</button>
      </div>
    </div>
    <pre ref="gutter" class="json-code-gutter" aria-hidden="true"><span
      v-for="number in lineNumbers"
      :key="number"
    >{{ number }}</span></pre>
    <pre
      v-if="readonly"
      ref="readonlyContent"
      class="json-code-readonly"
      :aria-label="label"
      tabindex="0"
      @scroll="syncScroll"
    ><code v-if="renderSyntax"><span
      v-for="line in lines"
      :key="line.id"
      class="syntax-line"
    ><span
      v-for="(token, tokenIndex) in line.tokens"
      :key="`${token.from}:${token.to}:${token.kind}`"
      :class="tokenClass(token.kind, line, tokenIndex)"
    >{{ line.text.slice(token.from, token.to) }}</span>{{ line.lineBreak }}</span></code><code v-else>{{ renderedValue }}</code></pre>
    <template v-else>
      <pre ref="highlight" class="json-code-highlight" aria-hidden="true"><code v-if="renderSyntax"><span
        v-for="line in lines"
        :key="line.id"
        class="syntax-line"
      ><span
        v-for="(token, tokenIndex) in line.tokens"
        :key="`${token.from}:${token.to}:${token.kind}`"
        :class="tokenClass(token.kind, line, tokenIndex)"
      >{{ line.text.slice(token.from, token.to) }}</span>{{ line.lineBreak }}</span></code><code v-else>{{ renderedValue }}</code></pre>
      <textarea
        ref="textarea"
        class="json-code-textarea"
        :value="textareaValue"
        :aria-label="label"
        autocomplete="off"
        autocapitalize="off"
        spellcheck="false"
        wrap="off"
        @input="update"
        @scroll="syncScroll"
        @keydown="handleKeydown"
      ></textarea>
    </template>
  </div>
</template>
