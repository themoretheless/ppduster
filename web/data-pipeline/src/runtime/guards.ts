import {
  RUNTIME_CONTRACT_VERSION,
  type RuntimeDiagnostic,
  type RuntimeWorkerReply,
  type RuntimeWorkerRequest,
} from "./contracts.ts";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

type EnvelopeCandidate = Record<string, unknown> & {
  type: string;
  payload: Record<string, unknown>;
};

function hasEnvelopeBase(value: unknown): value is EnvelopeCandidate {
  return (
    isRecord(value) &&
    value.contractVersion === RUNTIME_CONTRACT_VERSION &&
    typeof value.requestId === "string" &&
    value.requestId.length > 0 &&
    typeof value.type === "string" &&
    isRecord(value.payload)
  );
}

export function isRuntimeDiagnostic(value: unknown): value is RuntimeDiagnostic {
  if (!isRecord(value)) return false;

  const locationKeys = ["nodeId", "connectionId", "portId", "fieldPath"] as const;
  const validLocations = locationKeys.every(
    (key) => value[key] === undefined || typeof value[key] === "string",
  );

  return (
    typeof value.code === "string" &&
    value.code.length > 0 &&
    (value.severity === "info" || value.severity === "warning" || value.severity === "error") &&
    typeof value.message === "string" &&
    validLocations
  );
}

export function isRuntimeDiagnosticList(value: unknown): value is RuntimeDiagnostic[] {
  return Array.isArray(value) && value.every(isRuntimeDiagnostic);
}

/**
 * Checks the protocol discriminator and the identity fields at an untrusted
 * worker boundary. Payload validation remains the responsibility of the
 * command handler because graph and result payloads can be large.
 */
export function isRuntimeWorkerRequest(value: unknown): value is RuntimeWorkerRequest {
  if (!hasEnvelopeBase(value)) return false;

  switch (value.type) {
    case "compile":
      return isRecord(value.payload.graph);
    case "execute":
      return isRecord(value.payload.plan);
    case "cancel":
      return typeof value.payload.executionId === "string";
    default:
      return false;
  }
}

export function isRuntimeWorkerReply(value: unknown): value is RuntimeWorkerReply {
  if (!hasEnvelopeBase(value)) return false;

  switch (value.type) {
    case "compile.result":
      return (
        isRuntimeDiagnosticList(value.payload.diagnostics) &&
        (value.payload.plan === undefined || isRecord(value.payload.plan))
      );
    case "execute.started":
      return (
        typeof value.payload.executionId === "string" && typeof value.payload.planId === "string"
      );
    case "execute.node-result":
      return typeof value.payload.executionId === "string" && isRecord(value.payload.result);
    case "execute.result":
      return isRecord(value.payload);
    case "request.failed":
      return isRuntimeDiagnosticList(value.payload.diagnostics);
    default:
      return false;
  }
}
