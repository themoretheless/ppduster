import type {
  PlanFilterCondition,
  PlanFilterMode,
  PlanFilterOperator,
} from "./execute-plan.ts";

export const MAX_FILTER_EXPRESSION_DEPTH = 32;
export const MAX_FILTER_EXPRESSION_NODES = 256;

export type FilterQuantifier = "one" | "any" | "all" | "none";

export interface FilterConditionExpression {
  readonly kind: "condition";
  readonly field: string;
  /** How to combine values produced by `field` paths containing `[*]`. */
  readonly quantifier?: FilterQuantifier;
  readonly operator: PlanFilterOperator;
  readonly value?: string;
}

export interface FilterGroupExpression {
  readonly kind: "group";
  readonly operator: "and" | "or";
  readonly children: readonly FilterExpression[];
}

export interface FilterNotExpression {
  readonly kind: "not";
  readonly child: FilterExpression;
}

export type FilterExpression =
  | FilterConditionExpression
  | FilterGroupExpression
  | FilterNotExpression;

export interface FilterExpressionConditionEntry {
  readonly condition: FilterConditionExpression;
  /** Path relative to `config.expression`, matching Rust diagnostics. */
  readonly path: string;
}

const OPERATORS = new Set<PlanFilterOperator>([
  "equal", "not_equal", "greater_than", "greater_or_equal", "less_than",
  "less_or_equal", "contains", "starts_with", "ends_with", "exists", "not_exists",
]);
const QUANTIFIERS = new Set<FilterQuantifier>(["one", "any", "all", "none"]);

const isRecord = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

interface NormalizeBudget {
  nodes: number;
  readonly active: WeakSet<object>;
}

const normalizeAt = (
  input: unknown,
  depth: number,
  budget: NormalizeBudget,
): FilterExpression | null => {
  if (!isRecord(input) || depth > MAX_FILTER_EXPRESSION_DEPTH) return null;
  if (budget.active.has(input)) return null;
  budget.nodes += 1;
  if (budget.nodes > MAX_FILTER_EXPRESSION_NODES) return null;
  budget.active.add(input);

  let result: FilterExpression | null = null;
  if (
    input.kind === "condition" &&
    typeof input.field === "string" &&
    input.field.length > 0 &&
    typeof input.operator === "string" &&
    OPERATORS.has(input.operator as PlanFilterOperator) &&
    (input.quantifier === undefined ||
      (typeof input.quantifier === "string" && QUANTIFIERS.has(input.quantifier as FilterQuantifier))) &&
    (input.value === undefined || typeof input.value === "string")
  ) {
    result = {
      kind: "condition",
      field: input.field,
      ...(typeof input.quantifier === "string"
        ? { quantifier: input.quantifier as FilterQuantifier }
        : {}),
      operator: input.operator as PlanFilterOperator,
      ...(typeof input.value === "string" ? { value: input.value } : {}),
    };
  } else if (
    input.kind === "group" &&
    (input.operator === "and" || input.operator === "or") &&
    Array.isArray(input.children) &&
    input.children.length > 0
  ) {
    const children: FilterExpression[] = [];
    for (const child of input.children) {
      const normalized = normalizeAt(child, depth + 1, budget);
      if (!normalized) {
        budget.active.delete(input);
        return null;
      }
      children.push(normalized);
    }
    result = { kind: "group", operator: input.operator, children };
  } else if (input.kind === "not") {
    const child = normalizeAt(input.child, depth + 1, budget);
    if (child) result = { kind: "not", child };
  }

  budget.active.delete(input);
  return result;
};

/** Validates, depth-limits, cycle-checks and returns a canonical detached AST. */
export const normalizeFilterExpression = (input: unknown): FilterExpression | null =>
  normalizeAt(input, 1, { nodes: 0, active: new WeakSet() });

export const isFilterExpression = (input: unknown): input is FilterExpression =>
  normalizeFilterExpression(input) !== null;

/** Converts the v1 flat conditions without changing their all/any semantics. */
export const legacyConditionsToExpression = (
  conditions: readonly PlanFilterCondition[],
  mode: PlanFilterMode = "all",
): FilterExpression | null => {
  const children: FilterConditionExpression[] = [];
  for (const condition of conditions) {
    const normalized = normalizeFilterExpression({
      kind: "condition",
      field: condition.field,
      operator: condition.operator,
      ...(condition.value === undefined ? {} : { value: condition.value }),
    });
    if (!normalized || normalized.kind !== "condition") return null;
    children.push(normalized);
  }
  if (children.length === 0) return null;
  if (children.length === 1) return children[0];
  return { kind: "group", operator: mode === "any" ? "or" : "and", children };
};

/** Canonical JSON useful for cache keys; group child order is intentionally semantic. */
export const stableSerializeFilterExpression = (expression: FilterExpression): string => {
  const normalized = normalizeFilterExpression(expression);
  if (!normalized) throw new TypeError("Некорректное выражение фильтра");
  return JSON.stringify(normalized);
};

export const conditionsInFilterExpression = (
  expression: FilterExpression,
): readonly FilterExpressionConditionEntry[] => {
  const result: FilterExpressionConditionEntry[] = [];
  const visit = (node: FilterExpression, path: string): void => {
    if (node.kind === "condition") {
      result.push({ condition: node, path });
    } else if (node.kind === "not") {
      visit(node.child, `${path}.child`);
    } else {
      node.children.forEach((child, index) => visit(child, `${path}.children[${index}]`));
    }
  };
  visit(expression, "config.expression");
  return result;
};
