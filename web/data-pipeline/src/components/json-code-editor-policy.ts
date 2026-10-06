import type { TokenizedLine } from "@peregon/syntax-engine";

export type EditorEol = "\n" | "\r\n" | "\r";

// The editor is embedded in a small node card rather than a virtualized code
// viewport. Keep both source size and rendered span count bounded.
export const MAX_HIGHLIGHT_SOURCE_LENGTH = 128 * 1024;
export const MAX_HIGHLIGHT_TOKENS = 20_000;

export function dominantEol(value: string, fallback: EditorEol = "\n"): EditorEol {
  const counts = new Map<EditorEol, number>();
  const order: EditorEol[] = [];
  const seen = (eol: EditorEol) => {
    if (!counts.has(eol)) order.push(eol);
    counts.set(eol, (counts.get(eol) ?? 0) + 1);
  };

  for (let index = 0; index < value.length; index += 1) {
    if (value[index] === "\r") {
      if (value[index + 1] === "\n") {
        seen("\r\n");
        index += 1;
      } else seen("\r");
    } else if (value[index] === "\n") seen("\n");
  }

  let selected = fallback;
  let maximum = 0;
  for (const eol of order) {
    const count = counts.get(eol) ?? 0;
    if (count > maximum) {
      selected = eol;
      maximum = count;
    }
  }
  return selected;
}

/** Converts the textarea's LF-only API value to the model's EOL convention. */
export function withEditorEol(value: string, eol: EditorEol): string {
  const normalized = value.replace(/\r\n?|\n/gu, "\n");
  return eol === "\n" ? normalized : normalized.replaceAll("\n", eol);
}

export function canBuildSyntaxSnapshot(value: string, enabled: boolean): boolean {
  return enabled && value.length <= MAX_HIGHLIGHT_SOURCE_LENGTH;
}

export function exceedsHighlightTokenLimit(lines: readonly TokenizedLine[]): boolean {
  let count = 0;
  for (const line of lines) {
    count += line.tokens.length;
    if (count > MAX_HIGHLIGHT_TOKENS) return true;
  }
  return false;
}
