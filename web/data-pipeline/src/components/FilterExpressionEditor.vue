<script setup lang="ts">
import type { FilterOperator } from "../engine/types.ts";
import type { ExecutePlanFieldSchema } from "../runtime/execute-plan.ts";
import type { FilterQuantifier } from "../runtime/filter-expression.ts";
import {
  createUiCondition,
  createUiFilterGroup,
  type UiFilterExpression,
} from "../runtime/filter-ui.ts";

defineOptions({ name: "FilterExpressionEditor" });

const props = withDefaults(defineProps<{
  modelValue: UiFilterExpression;
  fields: readonly ExecutePlanFieldSchema[];
  operators: readonly { value: FilterOperator; label: string }[];
  depth?: number;
  removable?: boolean;
}>(), {
  depth: 0,
  removable: false,
});

const emit = defineEmits<{
  "update:modelValue": [value: UiFilterExpression];
  remove: [];
}>();

function updateCondition(patch: Partial<Extract<UiFilterExpression, { kind: "condition" }>>) {
  if (props.modelValue.kind !== "condition") return;
  emit("update:modelValue", { ...props.modelValue, ...patch });
}

function updateGroup(patch: Partial<Extract<UiFilterExpression, { kind: "group" }>>) {
  if (props.modelValue.kind !== "group") return;
  emit("update:modelValue", { ...props.modelValue, ...patch });
}

function updateNot(child: UiFilterExpression) {
  if (props.modelValue.kind !== "not") return;
  emit("update:modelValue", { ...props.modelValue, child });
}

function updateChild(index: number, child: UiFilterExpression) {
  if (props.modelValue.kind !== "group") return;
  const children = [...props.modelValue.children];
  children[index] = child;
  updateGroup({ children });
}

function removeChild(index: number) {
  if (props.modelValue.kind !== "group") return;
  updateGroup({ children: props.modelValue.children.filter((_, childIndex) => childIndex !== index) });
}

function defaultField() {
  return props.fields.find((field) => field.name === "state")?.name ?? props.fields[0]?.name ?? "state";
}

function addCondition() {
  if (props.modelValue.kind !== "group") return;
  updateGroup({ children: [...props.modelValue.children, createUiCondition(defaultField())] });
}

function addGroup() {
  if (props.modelValue.kind !== "group" || props.depth >= 7) return;
  updateGroup({ children: [...props.modelValue.children, createUiFilterGroup("and", defaultField())] });
}

function addNot() {
  if (props.modelValue.kind !== "group" || props.depth >= 7) return;
  updateGroup({
    children: [
      ...props.modelValue.children,
      { id: Date.now(), kind: "not", child: createUiCondition(defaultField()) },
    ],
  });
}

function needsValue(operator: FilterOperator) {
  return operator !== "exists" && operator !== "not_exists";
}

const quantifiers: readonly { value: FilterQuantifier; label: string }[] = [
  { value: "one", label: "одно" },
  { value: "any", label: "любое" },
  { value: "all", label: "все" },
  { value: "none", label: "ни одного" },
];
</script>

<template>
  <div class="filter-expression" :class="[`filter-expression-${modelValue.kind}`, { nested: depth > 0 }]">
    <template v-if="modelValue.kind === 'group'">
      <div class="filter-group-heading">
        <span>Группа</span>
        <div class="filter-logic-switch">
          <button type="button" :class="{ active: modelValue.operator === 'and' }" @click="updateGroup({ operator: 'and' })">И</button>
          <button type="button" :class="{ active: modelValue.operator === 'or' }" @click="updateGroup({ operator: 'or' })">ИЛИ</button>
        </div>
        <button v-if="removable" type="button" class="filter-expression-remove" aria-label="Удалить группу" @click="emit('remove')">×</button>
      </div>
      <div class="filter-group-children">
        <FilterExpressionEditor
          v-for="(child, index) in modelValue.children"
          :key="child.id"
          :model-value="child"
          :fields="fields"
          :operators="operators"
          :depth="depth + 1"
          removable
          @update:model-value="updateChild(index, $event)"
          @remove="removeChild(index)"
        />
      </div>
      <div class="filter-group-actions">
        <button type="button" @click="addCondition">+ Условие</button>
        <button type="button" :disabled="depth >= 7" @click="addGroup">+ Группа</button>
        <button type="button" :disabled="depth >= 7" @click="addNot">+ НЕ</button>
      </div>
    </template>

    <template v-else-if="modelValue.kind === 'not'">
      <div class="filter-not-heading">
        <strong>НЕ</strong>
        <button v-if="removable" type="button" class="filter-expression-remove" aria-label="Удалить отрицание" @click="emit('remove')">×</button>
      </div>
      <FilterExpressionEditor
        :model-value="modelValue.child"
        :fields="fields"
        :operators="operators"
        :depth="depth + 1"
        @update:model-value="updateNot"
      />
    </template>

    <template v-else>
      <span class="filter-expression-index">{{ depth }}</span>
      <input
        class="filter-path"
        :value="modelValue.field"
        :list="`filter-fields-${modelValue.id}`"
        aria-label="Путь поля условия"
        placeholder="profile.age или tags[*]"
        @input="updateCondition({ field: ($event.target as HTMLInputElement).value })"
      />
      <datalist :id="`filter-fields-${modelValue.id}`">
        <option v-for="field in fields" :key="field.name" :value="field.name" />
      </datalist>
      <select
        class="filter-quantifier"
        :value="modelValue.quantifier"
        aria-label="Квантор значений пути"
        @change="updateCondition({ quantifier: ($event.target as HTMLSelectElement).value as FilterQuantifier })"
      >
        <option v-for="quantifier in quantifiers" :key="quantifier.value" :value="quantifier.value">{{ quantifier.label }}</option>
      </select>
      <select class="filter-operator" :value="modelValue.operator" aria-label="Оператор условия" @change="updateCondition({ operator: ($event.target as HTMLSelectElement).value as FilterOperator })">
        <option v-for="operator in operators" :key="operator.value" :value="operator.value">{{ operator.label }}</option>
      </select>
      <input
        v-if="needsValue(modelValue.operator)"
        class="filter-value"
        :value="modelValue.value"
        aria-label="Значение условия"
        placeholder="значение"
        @input="updateCondition({ value: ($event.target as HTMLInputElement).value })"
      />
      <span v-else class="unary-value">без значения</span>
      <button v-if="removable" type="button" class="filter-expression-remove" aria-label="Удалить условие" @click="emit('remove')">×</button>
    </template>
  </div>
</template>
