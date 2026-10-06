export interface ValueToken {
  readonly position: number;
  readonly value: string;
  readonly sourceStart: number;
  readonly sourceEnd: number;
  readonly quoted: boolean;
}

export class ValueVectorParseError extends Error {
  readonly offset: number;

  constructor(message: string, offset: number) {
    super(message);
    this.name = "ValueVectorParseError";
    this.offset = offset;
  }
}

/** Parses comma/newline separated scalar values without constructing JSON records. */
export function parseValueVector(source: string): ValueToken[] {
  const tokens: ValueToken[] = [];
  let value = "";
  let tokenStart = 0;
  let quote: "\"" | "'" | null = null;
  let quoted = false;

  const push = (end: number) => {
    const normalized = value.trim();
    if (normalized) {
      tokens.push({
        position: tokens.length,
        value: normalized,
        sourceStart: tokenStart,
        sourceEnd: end,
        quoted,
      });
    }
    value = "";
    quoted = false;
    tokenStart = end + 1;
  };

  for (let index = 0; index < source.length; index += 1) {
    const character = source[index];
    if (quote) {
      if (character === "\\" && source[index + 1] === quote) {
        value += quote;
        index += 1;
      } else if (character === quote) {
        quote = null;
      } else {
        value += character;
      }
      continue;
    }

    if ((character === "\"" || character === "'") && !value.trim()) {
      quote = character;
      quoted = true;
    } else if (character === "," || character === "\n") {
      push(index);
    } else if (character !== "\r") {
      value += character;
    }
  }

  if (quote) throw new ValueVectorParseError("Незакрытая кавычка в списке", source.length);
  push(source.length);
  return tokens;
}
