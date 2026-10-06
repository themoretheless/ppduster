import assert from "node:assert/strict";
import test from "node:test";

import {
  RUNTIME_CONTRACT_VERSION,
  isRuntimeDiagnostic,
  isRuntimeDiagnosticList,
  isRuntimeWorkerReply,
  isRuntimeWorkerRequest,
} from "../src/runtime/index.ts";

const envelope = (type, payload, overrides = {}) => ({
  contractVersion: RUNTIME_CONTRACT_VERSION,
  requestId: "request-1",
  type,
  payload,
  ...overrides,
});

test("runtime request guard accepts every supported request discriminator", () => {
  assert.equal(
    isRuntimeWorkerRequest(envelope("compile", { graph: { version: 2 } })),
    true,
  );
  assert.equal(
    isRuntimeWorkerRequest(envelope("execute", { plan: { contractVersion: 1 } })),
    true,
  );
  assert.equal(
    isRuntimeWorkerRequest(envelope("cancel", { executionId: "execution-1" })),
    true,
  );
});

test("runtime request guard rejects invalid envelopes and command payloads", async (t) => {
  const cases = [
    ["null", null],
    ["wrong protocol version", envelope("cancel", { executionId: "x" }, { contractVersion: 2 })],
    ["empty request id", envelope("cancel", { executionId: "x" }, { requestId: "" })],
    ["unknown discriminator", envelope("unknown", {})],
    ["array payload", envelope("compile", [])],
    ["compile without graph", envelope("compile", {})],
    ["execute without plan", envelope("execute", {})],
    ["cancel without execution id", envelope("cancel", {})],
  ];

  for (const [name, value] of cases) {
    await t.test(name, () => assert.equal(isRuntimeWorkerRequest(value), false));
  }
});

test("runtime diagnostic guards validate severity and optional locations", () => {
  const diagnostic = {
    code: "missing_field",
    severity: "error",
    message: "Field /state is not available",
    nodeId: "filter",
    fieldPath: "/state",
  };

  assert.equal(isRuntimeDiagnostic(diagnostic), true);
  assert.equal(isRuntimeDiagnosticList([diagnostic]), true);
  assert.equal(isRuntimeDiagnostic({ ...diagnostic, severity: "fatal" }), false);
  assert.equal(isRuntimeDiagnostic({ ...diagnostic, nodeId: 42 }), false);
  assert.equal(isRuntimeDiagnosticList([diagnostic, null]), false);
});

test("runtime reply guard accepts supported replies and rejects malformed ones", () => {
  const diagnostic = {
    code: "invalid_graph",
    severity: "error",
    message: "Graph is invalid",
  };

  assert.equal(
    isRuntimeWorkerReply(envelope("compile.result", { diagnostics: [], plan: {} })),
    true,
  );
  assert.equal(
    isRuntimeWorkerReply(
      envelope("execute.started", { executionId: "execution-1", planId: "plan-1" }),
    ),
    true,
  );
  assert.equal(
    isRuntimeWorkerReply(
      envelope("execute.node-result", { executionId: "execution-1", result: {} }),
    ),
    true,
  );
  assert.equal(isRuntimeWorkerReply(envelope("execute.result", { status: "completed" })), true);
  assert.equal(
    isRuntimeWorkerReply(envelope("request.failed", { diagnostics: [diagnostic] })),
    true,
  );

  assert.equal(
    isRuntimeWorkerReply(envelope("compile.result", { diagnostics: "not-an-array" })),
    false,
  );
  assert.equal(
    isRuntimeWorkerReply(envelope("execute.started", { executionId: "execution-1" })),
    false,
  );
  assert.equal(
    isRuntimeWorkerReply(envelope("request.failed", { diagnostics: [{ ...diagnostic, code: "" }] })),
    false,
  );
});
