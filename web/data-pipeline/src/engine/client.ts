import type { EngineRequest, TimedResponse } from "./types";

interface WorkerSuccess<T> extends TimedResponse<T> {
  id: number;
}

interface WorkerFailure {
  id: number;
  error: string;
}

export class JsonEngineClient {
  private readonly worker = new Worker(new URL("./peregon.worker.ts", import.meta.url), {
    type: "module",
    name: "peregon-wasm",
  });

  private nextId = 1;
  private pending = new Map<
    number,
    {
      resolve: (value: TimedResponse<unknown>) => void;
      reject: (reason: Error) => void;
    }
  >();

  constructor() {
    this.worker.onmessage = (event: MessageEvent<WorkerSuccess<unknown> | WorkerFailure>) => {
      const message = event.data;
      const promise = this.pending.get(message.id);
      if (!promise) return;

      this.pending.delete(message.id);
      if ("error" in message) {
        promise.reject(new Error(message.error));
      } else {
        promise.resolve({
          response: message.response,
          durationMs: message.durationMs,
          engineVersion: message.engineVersion,
        });
      }
    };

    this.worker.onerror = (event) => {
      const error = new Error(event.message || "Ошибка фонового WASM-процесса");
      for (const promise of this.pending.values()) promise.reject(error);
      this.pending.clear();
    };
  }

  request<T>(request: EngineRequest): Promise<TimedResponse<T>> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (value: TimedResponse<unknown>) => void,
        reject,
      });
      this.worker.postMessage({ id, request });
    });
  }

  terminate() {
    this.worker.terminate();
    const error = new Error("WASM-процесс остановлен");
    for (const promise of this.pending.values()) promise.reject(error);
    this.pending.clear();
  }
}
