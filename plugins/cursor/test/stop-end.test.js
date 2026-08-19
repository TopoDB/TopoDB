import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readState, appendRetrieval } from "../core/recorder.js";
import { NUDGE_TEXT } from "../core/hooks/stop.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STOP = path.join(HERE, "..", "hooks", "stop-nudge.js");
const END = path.join(HERE, "..", "hooks", "session-end.js");
const run = (hook, payload, env) => execFileSync(process.execPath, [hook], { input: JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();
// SYNTHETIC Cursor transcript (spec D4 replaces it): 5 tool calls.
const transcript = (n) => Array.from({ length: n }, () => JSON.stringify({ role: "assistant", content: [{ type: "text", text: "t" }, { type: "tool_call", name: "Shell" }] })).join("\n");

test("stop: nudges once for a substantive completed session, never twice, not on abort/loop/short/unreadable", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-stop-"));
  try {
    const tp = path.join(dir, "t.jsonl"); writeFileSync(tp, transcript(5));
    const base = { hook_event_name: "stop", conversation_id: "c1", workspace_roots: [dir], transcript_path: tp, status: "completed", loop_count: 0 };
    const env = { TOPODB_PLUGIN_DATA: dir };
    assert.deepEqual(JSON.parse(run(STOP, base, env)), { followup_message: NUDGE_TEXT });
    assert.equal(readState(dir, "c1").nudged, true);
    assert.equal(run(STOP, base, env), "");                                            // once per session
    assert.equal(run(STOP, { ...base, conversation_id: "c2", status: "aborted" }, env), "");
    assert.equal(run(STOP, { ...base, conversation_id: "c3", loop_count: 1 }, env), "");
    writeFileSync(tp, transcript(4));
    assert.equal(run(STOP, { ...base, conversation_id: "c4" }, env), "");              // < 5 tools
    assert.equal(run(STOP, { ...base, conversation_id: "c5", transcript_path: path.join(dir, "missing") }, env), "");
    writeFileSync(tp, transcript(5));
    assert.equal(run(STOP, { ...base, conversation_id: "c6" }, { ...env, TOPODB_RECORDING: "0" }), "");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("sessionEnd: writes a session_end marker; with no daemon the episode state is kept; background agents ignored", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-end-"));
  try {
    appendRetrieval(dir, "e1", { tool: "search_memories", query: "q", at: 1, channel: "text", returned: [{ id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", rank: 1, score: 0.5 }] }, new Map([["01ARZ3NDEKTSV4RRFFQ69G5FAV", "x"]]));
    const out = run(END, { hook_event_name: "sessionEnd", session_id: "e1", conversation_id: "e1", workspace_roots: [dir], reason: "user_close", is_background_agent: false }, { TOPODB_PLUGIN_DATA: dir });
    assert.equal(out, "");
    assert.ok(readState(dir, "e1"), "state kept when no daemon is reachable");
    const spool = path.join(dir, "memory.warehouse", "spool");
    const evs = readdirSync(spool).flatMap((f) => readFileSync(path.join(spool, f), "utf8").split("\n").filter(Boolean).map(JSON.parse));
    assert.ok(evs.some((e) => e.marker?.type === "session_end" && e.marker.harness === "cursor"));
    assert.equal(run(END, { session_id: "bg", workspace_roots: [dir], is_background_agent: true }, { TOPODB_PLUGIN_DATA: dir }), "");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
