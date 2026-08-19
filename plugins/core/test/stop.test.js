import { test } from "node:test";
import assert from "node:assert/strict";
import { nudgeGate, NUDGE_TEXT, SUBSTANTIVE_MIN_TOOLS } from "../hooks/stop.js";

const ok = { dataDir: "/d", env: {}, sessionId: "s", state: null, toolUses: 5 };
test("nudgeGate", () => {
  assert.equal(nudgeGate(ok), true);
  assert.equal(nudgeGate({ ...ok, dataDir: null }), false);
  assert.equal(nudgeGate({ ...ok, env: { TOPODB_RECORDING: "0" } }), false);
  assert.equal(nudgeGate({ ...ok, env: { TOPODB_CAPTURE_NUDGE: "off" } }), false);
  assert.equal(nudgeGate({ ...ok, sessionId: "" }), false);
  assert.equal(nudgeGate({ ...ok, state: { nudged: true } }), false);
  assert.equal(nudgeGate({ ...ok, state: { captured: true } }), false);
  assert.equal(nudgeGate({ ...ok, toolUses: SUBSTANTIVE_MIN_TOOLS - 1 }), false);
  assert.match(NUDGE_TEXT, /remember/);
});
