<script setup lang="ts">
import { computed, ref } from "vue";

const props = defineProps<{
  value: string;
  label: string;
}>();

const content = ref<HTMLElement | null>(null);
const gutter = ref<HTMLElement | null>(null);
const copied = ref(false);
let copiedTimer: ReturnType<typeof setTimeout> | undefined;

const lineNumbers = computed(() => Array.from(
  { length: Math.max(1, props.value.split("\n").length) },
  (_, index) => index + 1,
));

function syncScroll() {
  if (content.value && gutter.value) gutter.value.scrollTop = content.value.scrollTop;
}

async function copyAll() {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(props.value);
  } else {
    const fallback = document.createElement("textarea");
    fallback.value = props.value;
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
</script>

<template>
  <div class="plain-text-viewer node-result-output">
    <div class="plain-text-toolbar">
      <span>Текст · {{ lineNumbers.length }} строк</span>
      <button
        type="button"
        title="Копировать весь текст"
        @pointerdown.stop
        @click.stop="copyAll"
      >{{ copied ? "Скопировано" : "Копировать всё" }}</button>
    </div>
    <pre ref="gutter" class="plain-text-gutter" aria-hidden="true"><span
      v-for="number in lineNumbers"
      :key="number"
    >{{ number }}</span></pre>
    <pre
      ref="content"
      class="plain-text-content"
      :aria-label="label"
      tabindex="0"
      @scroll="syncScroll"
    >{{ value }}</pre>
  </div>
</template>
