import type { ExecutePlanInput, ExecutePlanStep } from "./execute-plan.ts";

export const EXECUTE_PLAN_CACHE_VERSION = "v1" as const;

const CACHE_SALT = `peregon:execute-plan-step:${EXECUTE_PLAN_CACHE_VERSION}`;
const NON_SEMANTIC_KEYS = new Set([
  "cache",
  "cache_key",
  "output",
  "output_schema",
  "position",
  "preview",
  "previewError",
  "previewStats",
  "runtime",
  "stats",
  "title",
]);

export interface CacheKeyStepInput {
  readonly node_type: ExecutePlanStep["node_type"];
  readonly config: object;
  readonly input?: ExecutePlanInput;
}

/**
 * Produces detached JSON-compatible data with lexically sorted object keys.
 * Arrays keep their order because expression order and projected field order
 * are semantic. Non-execution UI/runtime state is omitted recursively.
 */
export const canonicalizeForCache = (value: unknown): unknown => {
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("Cache config contains a non-finite number");
    return Object.is(value, -0) ? 0 : value;
  }
  if (Array.isArray(value)) {
    return value.map((item) => item === undefined ? null : canonicalizeForCache(item));
  }
  if (typeof value === "object") {
    const source = value as Readonly<Record<string, unknown>>;
    const result: Record<string, unknown> = {};
    for (const key of Object.keys(source).sort()) {
      if (NON_SEMANTIC_KEYS.has(key) || source[key] === undefined) continue;
      result[key] = canonicalizeForCache(source[key]);
    }
    return result;
  }
  throw new TypeError(`Cache config contains unsupported ${typeof value}`);
};

export const stableSerializeForCache = (value: unknown): string =>
  JSON.stringify(canonicalizeForCache(value));

const fnv1a64 = (value: string): string => {
  let hash = 0xcbf29ce484222325n;
  const prime = 0x100000001b3n;
  for (const byte of new TextEncoder().encode(value)) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * prime);
  }
  return hash.toString(16).padStart(16, "0");
};

/** Computes an opaque key whose lineage stops at the immediate parent. */
export const computeExecutePlanStepCacheKey = (
  step: CacheKeyStepInput,
  parentCacheKey?: string,
): string => {
  if (step.input && !parentCacheKey) {
    throw new TypeError(`Parent cache key is required for ${step.node_type}`);
  }
  const identity = {
    salt: CACHE_SALT,
    node_type: step.node_type,
    config: step.config,
    input: step.input
      ? { parent_key: parentCacheKey, port: step.input.port }
      : null,
  };
  return `${EXECUTE_PLAN_CACHE_VERSION}-${fnv1a64(stableSerializeForCache(identity))}`;
};
