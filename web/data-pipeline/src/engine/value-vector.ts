import {
  AsyncDuckDB,
  VoidLogger,
  type AsyncDuckDBConnection,
} from "@duckdb/duckdb-wasm";
import duckdbMvpUrl from "@duckdb/duckdb-wasm/dist/duckdb-mvp.wasm?url";
import duckdbWorkerSource from "@duckdb/duckdb-wasm/dist/duckdb-browser-mvp.worker.js?raw";
import { tableFromArrays } from "apache-arrow";
import type {
  ExecutePlanNodeResult,
  ExecutePlanRequest,
  ExecutePlanResponse,
  ExecutePlanStep,
  ExecuteSourceStep,
} from "../runtime/execute-plan.ts";
import { parseValueVector, type ValueToken } from "./value-vector-parser.ts";

interface VectorState {
  readonly tableName: string;
  readonly count: number;
}

let duckdbPromise: Promise<AsyncDuckDB> | undefined;
let requestSequence = 0;

const vectorSchema = { kind: "value-vector" as const, value_type: "string" as const };

const withTimeout = async <T>(label: string, operation: Promise<T>): Promise<T> => {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  const timeoutPromise = new Promise<never>((_, reject) => {
    timeout = setTimeout(() => reject(new Error(`${label}: превышено время ожидания`)), 15_000);
  });
  try {
    return await Promise.race([operation, timeoutPromise]);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`${label}: ${message}`);
  } finally {
    if (timeout) clearTimeout(timeout);
  }
};

const duckdb = (): Promise<AsyncDuckDB> => {
  duckdbPromise ??= (async () => {
    const workerUrl = URL.createObjectURL(new Blob([duckdbWorkerSource], { type: "text/javascript" }));
    try {
      const database = new AsyncDuckDB(new VoidLogger(), new Worker(workerUrl));
      const wasmUrl = new URL(duckdbMvpUrl, self.location.href).href;
      await withTimeout("Инициализация DuckDB", database.instantiate(wasmUrl));
      await withTimeout("Открытие DuckDB", database.open({ path: ":memory:" }));
      return database;
    } finally {
      URL.revokeObjectURL(workerUrl);
    }
  })();
  return duckdbPromise;
};

const isVectorSource = (step: ExecutePlanStep): step is ExecuteSourceStep =>
  step.node_type === "source" && step.config.format === "list";

const isVectorSink = (step: ExecutePlanStep): boolean =>
  step.node_type === "sink" && (step.config.format === "template" || step.config.format === "flat");

export const isValueVectorPlan = (request: ExecutePlanRequest): boolean =>
  request.plan.steps.length > 0 && request.plan.steps.every((step) =>
    isVectorSource(step) || step.node_type === "template" || isVectorSink(step));

const stats = (input: number, output: number, values = output) => ({
  input_items: input,
  output_items: output,
  filtered_out: 0,
  skipped_items: 0,
  empty_values: 0,
  values,
});

const nodeResult = (
  preview: readonly string[],
  inputSchema: typeof vectorSchema | null,
  outputSchema: typeof vectorSchema | null,
  inputTotal: number,
  outputTotal: number,
  previewLimit: number,
): ExecutePlanNodeResult => ({
  ok: true,
  cached: false,
  preview: preview.slice(0, previewLimit),
  preview_truncated: outputTotal > previewLimit,
  schema: outputSchema,
  input_schema: inputSchema,
  output_schema: outputSchema,
  stats: stats(inputSchema ? inputTotal : 0, outputTotal),
  diagnostics: [],
});

const insertVector = async (
  connection: AsyncDuckDBConnection,
  tableName: string,
  tokens: readonly ValueToken[],
) => {
  if (tokens.length === 0) {
    await connection.query(`
      CREATE TABLE ${tableName} (
        position UINTEGER,
        value VARCHAR,
        source_start UINTEGER,
        source_end UINTEGER,
        quoted BOOLEAN
      )
    `);
    return;
  }
  const table = tableFromArrays({
    position: Uint32Array.from(tokens, (token) => token.position),
    value: tokens.map((token) => token.value),
    source_start: Uint32Array.from(tokens, (token) => token.sourceStart),
    source_end: Uint32Array.from(tokens, (token) => token.sourceEnd),
    quoted: tokens.map((token) => token.quoted),
  });
  await connection.insertArrowTable(table, { name: tableName, create: true });
};

const orderedValues = async (connection: AsyncDuckDBConnection, tableName: string): Promise<string[]> =>
  (await connection.query(`SELECT value FROM ${tableName} ORDER BY position`))
    .toArray()
    .map((row) => String((row as unknown as { value: unknown }).value));

const tokensFromValues = (values: readonly string[]): ValueToken[] => values.map((value, position) => ({
  position,
  value,
  sourceStart: 0,
  sourceEnd: 0,
  quoted: false,
}));

const applyTemplate = async (
  connection: AsyncDuckDBConnection,
  tableName: string,
  template: string,
  stripOuterQuotes: boolean,
  skipEmpty: boolean,
): Promise<string[]> => {
  const statement = await connection.prepare(`
    WITH normalized AS (
      SELECT
        position,
        CASE
          WHEN CAST(? AS BOOLEAN) AND length(value) >= 2
            AND left(value, 1) = right(value, 1)
            AND left(value, 1) IN ('"', '''')
          THEN substr(value, 2, length(value) - 2)
          ELSE value
        END AS value
      FROM ${tableName}
    )
    SELECT replace(CAST(? AS VARCHAR), '{value}', value) AS output
    FROM normalized
    WHERE NOT CAST(? AS BOOLEAN) OR value <> ''
    ORDER BY position
  `);
  try {
    const result = await statement.query(stripOuterQuotes, template, skipEmpty);
    return result.toArray()
      .map((row) => String((row as unknown as { output: unknown }).output));
  } finally {
    await statement.close();
  }
};

export async function executeValueVectorPlan(request: ExecutePlanRequest): Promise<ExecutePlanResponse> {
  const bindings = await duckdb();
  const connection = await bindings.connect();
  const requestId = ++requestSequence;
  const states = new Map<string, VectorState>();
  const nodes: Record<string, ExecutePlanNodeResult> = {};
  const sinkOutputs: Record<string, string> = {};
  const createdTables: string[] = [];

  try {
    for (const step of request.plan.steps) {
      if (isVectorSource(step)) {
        const tokens = parseValueVector(step.config.data);
        const tableName = `value_vector_${requestId}_${createdTables.length}`;
        await withTimeout("Загрузка вектора в DuckDB", insertVector(connection, tableName, tokens));
        createdTables.push(tableName);
        const values = await withTimeout("Чтение вектора из DuckDB", orderedValues(connection, tableName));
        states.set(step.node_id, { tableName, count: tokens.length });
        nodes[step.node_id] = nodeResult(values, null, vectorSchema, 0, tokens.length, request.plan.preview_limit);
        continue;
      }

      if (step.node_type === "template") {
        const parent = states.get(step.input.node_id);
        if (!parent) throw new Error(`Векторный вход блока «${step.node_id}» не найден`);
        const values = await withTimeout("Применение шаблона в DuckDB", applyTemplate(
          connection,
          parent.tableName,
          step.config.value_template,
          step.config.strip_outer_quotes,
          step.config.skip_empty,
        ));
        const tableName = `value_vector_${requestId}_${createdTables.length}`;
        await withTimeout("Сохранение результата шаблона", insertVector(connection, tableName, tokensFromValues(values)));
        createdTables.push(tableName);
        states.set(step.node_id, { tableName, count: values.length });
        nodes[step.node_id] = nodeResult(
          values,
          vectorSchema,
          vectorSchema,
          parent.count,
          values.length,
          request.plan.preview_limit,
        );
        continue;
      }

      if (step.node_type === "sink" && isVectorSink(step)) {
        const parent = states.get(step.input.node_id);
        if (!parent) throw new Error(`Векторный вход блока «${step.node_id}» не найден`);
        const values = await withTimeout("Чтение входа шаблона", orderedValues(connection, parent.tableName));
        const rendered = step.config.format === "template"
          ? await withTimeout("Применение шаблона в DuckDB", applyTemplate(
            connection,
            parent.tableName,
            step.config.value_template,
            step.config.strip_outer_quotes,
            step.config.skip_empty,
          ))
          : values;
        nodes[step.node_id] = nodeResult(
          rendered,
          vectorSchema,
          null,
          parent.count,
          rendered.length,
          request.plan.preview_limit,
        );
        sinkOutputs[step.node_id] = rendered.join(step.config.delimiter);
      }
    }

    return { ok: true, nodes, sink_outputs: sinkOutputs, diagnostics: [] };
  } finally {
    for (const tableName of createdTables) {
      try {
        await connection.query(`DROP TABLE IF EXISTS ${tableName}`);
      } catch {
        // The in-memory database is request-scoped by table name; cleanup must not hide the real error.
      }
    }
    try {
      await connection.close();
    } catch {
      // Likewise, preserve the original execution error if DuckDB has already rejected the connection.
    }
  }
}
