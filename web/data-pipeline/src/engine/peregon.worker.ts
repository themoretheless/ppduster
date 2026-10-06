/// <reference lib="webworker" />

import init, {
  engine_version,
  process_request,
} from "../generated/peregon_engine/peregon_engine.js";
import type { EngineRequest } from "./types";
import type { ExecutePlanRequest, ExecutePlanResponse, ExecutePlanStep } from "../runtime/execute-plan.ts";
import { executeValueVectorPlan, isValueVectorPlan } from "./value-vector.ts";

interface IncomingMessage {
  id: number;
  request: EngineRequest;
}

const wasmReady = init();

const requestWithSteps = (
  request: ExecutePlanRequest,
  steps: readonly ExecutePlanStep[],
): ExecutePlanRequest => ({
  ...request,
  plan: { ...request.plan, steps },
});

const partitionPlan = (request: ExecutePlanRequest) => {
  const vectorNodeIds = new Set<string>();
  for (const step of request.plan.steps) {
    if (step.node_type === "source" && step.config.format === "list") {
      vectorNodeIds.add(step.node_id);
      continue;
    }
    if (
      step.input && vectorNodeIds.has(step.input.node_id)
      && (step.node_type === "template" || step.node_type === "sink")
    ) {
      vectorNodeIds.add(step.node_id);
    }
  }
  return {
    vector: request.plan.steps.filter((step) => vectorNodeIds.has(step.node_id)),
    records: request.plan.steps.filter((step) => !vectorNodeIds.has(step.node_id)),
  };
};

const executeWasmPlan = async (request: ExecutePlanRequest): Promise<ExecutePlanResponse> => {
  await wasmReady;
  return JSON.parse(process_request(JSON.stringify(request))) as ExecutePlanResponse;
};

const executePlan = async (request: ExecutePlanRequest): Promise<ExecutePlanResponse> => {
  if (isValueVectorPlan(request)) return executeValueVectorPlan(request);
  const partition = partitionPlan(request);
  if (!partition.vector.length) return executeWasmPlan(request);
  const vectorResponse = await executeValueVectorPlan(requestWithSteps(request, partition.vector));
  if (!partition.records.length) return vectorResponse;
  const recordResponse = await executeWasmPlan(requestWithSteps(request, partition.records));
  return {
    ok: vectorResponse.ok && recordResponse.ok,
    nodes: { ...recordResponse.nodes, ...vectorResponse.nodes },
    sink_outputs: { ...recordResponse.sink_outputs, ...vectorResponse.sink_outputs },
    diagnostics: [...recordResponse.diagnostics, ...vectorResponse.diagnostics],
  };
};

self.onmessage = async (event: MessageEvent<IncomingMessage>) => {
  const { id, request } = event.data;
  const startedAt = performance.now();

  try {
    const response = request.action === "execute_plan"
      ? await executePlan(request)
      : (await wasmReady, JSON.parse(process_request(JSON.stringify(request))));
    self.postMessage({
      id,
      response,
      durationMs: performance.now() - startedAt,
      engineVersion: engine_version(),
    });
  } catch (error) {
    self.postMessage({
      id,
      error: error instanceof Error ? error.message : "WASM-модуль не ответил",
    });
  }
};
