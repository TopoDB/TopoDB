import { test } from "node:test";
import assert from "node:assert/strict";
import { countToolUses, decideNudge } from "../hooks/stop-capture.js";

const jsonl = (...objs) => objs.map((o) => JSON.stringify(o)).join("\n");
const assistantWithTools = (n) => ({
  type: "assistant",
  message: { role: "assistant", content: Array.from({ length: n }, () => ({ type: "tool_use", name: "Bash" })) },
});

test("countToolUses: sums tool_use blocks across assistant messages, ignores text/other", () => {
  const t = jsonl(
    assistantWithTools(2),
    { type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "hi" }] } },
    assistantWithTools(3),
    { type: "user", message: { role: "user", content: "x" } },
  );
  assert.equal(countToolUses(t), 5);
  assert.equal(countToolUses(""), 0);
  assert.equal(countToolUses("{bad"), 0);
});

// A payload/env/state that WOULD nudge; each test flips one gate off.
const base = {
  payload: { session_id: "s", permission_mode: "default", stop_hook_active: false },
  env: { CLAUDE_PLUGIN_DATA: "/data" },
  state: null,
  toolUses: 5,
};

test("decideNudge: happy path (all gates pass) → true", () => {
  assert.equal(decideNudge(base), true);
});

test("decideNudge: each gate blocks the nudge", () => {
  assert.equal(decideNudge({ ...base, payload: { ...base.payload, stop_hook_active: true } }), false, "stop_hook_active");
  assert.equal(decideNudge({ ...base, payload: { ...base.payload, agent_id: "a" } }), false, "subagent");
  assert.equal(decideNudge({ ...base, payload: { ...base.payload, permission_mode: "plan" } }), false, "plan mode");
  assert.equal(decideNudge({ ...base, env: {} }), false, "no dataDir");
  assert.equal(decideNudge({ ...base, env: { ...base.env, TOPODB_RECORDING: "off" } }), false, "recording off");
  assert.equal(decideNudge({ ...base, env: { ...base.env, TOPODB_CAPTURE_NUDGE: "0" } }), false, "nudge off");
  assert.equal(decideNudge({ ...base, payload: { ...base.payload, session_id: undefined } }), false, "no session");
  assert.equal(decideNudge({ ...base, state: { nudged: true } }), false, "already nudged");
  assert.equal(decideNudge({ ...base, state: { captured: true } }), false, "already captured");
  assert.equal(decideNudge({ ...base, toolUses: 4 }), false, "not substantive");
});
