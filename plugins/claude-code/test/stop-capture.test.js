import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { countToolUses, decideNudge } from "../hooks/stop-capture.js";
import { readState } from "../core/recorder.js";

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

// Integration tests: spawned hook scripts with real filesystem

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.join(HERE, "..");
const STOP_HOOK = path.join(PLUGIN_ROOT, "hooks", "stop-capture.js");
const MARK_HOOK = path.join(PLUGIN_ROOT, "hooks", "mark-captured.js");

function runHook(hookPath, payload, extraEnv = {}) {
  return new Promise((resolve) => {
    const p = spawn("node", [hookPath], { env: { ...process.env, ...extraEnv } });
    let buf = "";
    p.stdout.on("data", (d) => (buf += d));
    p.on("close", () => resolve(buf.trim()));
    p.stdin.end(JSON.stringify(payload));
  });
}

// transcript with N tool_use blocks
function tmpTranscript(dir, n) {
  const t = path.join(dir, "t.jsonl");
  const line = JSON.stringify({ type: "assistant", message: { role: "assistant", content: Array.from({ length: n }, () => ({ type: "tool_use", name: "Bash" })) } });
  writeFileSync(t, line + "\n");
  return t;
}

test("integration: substantive session with no prior capture → block nudge + marks nudged", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  const t = tmpTranscript(dir, 6);
  const out = await runHook(STOP_HOOK, { session_id: "s1", permission_mode: "default", stop_hook_active: false, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir, TOPODB_CAPTURE_NUDGE: "", TOPODB_RECORDING: "" });
  const parsed = JSON.parse(out);
  assert.equal(parsed.decision, "block");
  assert.match(parsed.reason, /remember/);
  assert.match(parsed.reason, /supersede/);
  assert.equal(readState(dir, "s1").nudged, true);
});

test("integration: stop_hook_active → no output (loop guard)", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  const t = tmpTranscript(dir, 6);
  const out = await runHook(STOP_HOOK, { session_id: "s2", permission_mode: "default", stop_hook_active: true, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir });
  assert.equal(out, "");
});

test("integration: non-substantive transcript → no output", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  const t = tmpTranscript(dir, 2);
  const out = await runHook(STOP_HOOK, { session_id: "s3", permission_mode: "default", stop_hook_active: false, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir });
  assert.equal(out, "");
});

test("integration: mark-captured flips captured, then stop-capture stays quiet", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  const t = tmpTranscript(dir, 6);
  await runHook(MARK_HOOK, { session_id: "s4", tool_name: "mcp__topodb__remember" }, { CLAUDE_PLUGIN_DATA: dir });
  assert.equal(readState(dir, "s4").captured, true);
  const out = await runHook(STOP_HOOK, { session_id: "s4", permission_mode: "default", stop_hook_active: false, transcript_path: t }, { CLAUDE_PLUGIN_DATA: dir });
  assert.equal(out, "", "captured session should not be nudged");
});
