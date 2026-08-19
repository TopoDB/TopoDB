import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readState } from "../core/recorder.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CAP = path.join(HERE, "..", "hooks", "warehouse-capture.js");
const MCP = path.join(HERE, "..", "hooks", "after-mcp.js");
const run = (hook, payload, env) => execFileSync(process.execPath, [hook], { input: JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();
const spooled = (dir) => { const s = path.join(dir, "memory.warehouse", "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)).sort((a, b) => a.ts - b.ts) : []; };

test("postToolUse: Shell/Read spooled with harness=cursor; MCP and unknown tools ignored; kill switch", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-cap-"));
  try {
    const base = { hook_event_name: "postToolUse", conversation_id: "conv-1", workspace_roots: [dir], cwd: dir };
    assert.equal(run(CAP, { ...base, tool_name: "Shell", tool_input: { command: "ls" }, tool_output: JSON.stringify({ output: "a" }) }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(run(CAP, { ...base, tool_name: "Read", tool_input: { path: "/x.rs" }, tool_output: "hello" }, { TOPODB_PLUGIN_DATA: dir, TOPODB_SESSION_ID: "conv-1" }), "");
    assert.equal(run(CAP, { ...base, tool_name: "MCP:topodb", tool_input: {}, tool_output: "{}" }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(run(CAP, { ...base, tool_name: "Task", tool_input: {}, tool_output: "{}" }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(run(CAP, { ...base, tool_name: "Shell", tool_input: { command: "pwd" }, tool_output: "/" }, { TOPODB_PLUGIN_DATA: dir, TOPODB_WAREHOUSE: "0" }), "");
    const evs = spooled(dir);
    assert.equal(evs.length, 2);
    assert.equal(evs[0].artifact.type, "command"); assert.equal(evs[0].source.harness, "cursor"); assert.equal(evs[0].source.session, "conv-1");
    assert.equal(evs[1].artifact.type, "file_read"); assert.equal(evs[1].artifact.content, "hello");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("afterMCPExecution: retrieval tools recorded into episode state; write tools mark captured + marker; others ignored", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-mcp-"));
  try {
    const base = { hook_event_name: "afterMCPExecution", conversation_id: "conv-2", workspace_roots: [dir] };
    const search = { hits: [{ node: { id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", props: { content: "hello" } }, score: 0.9 }] };
    assert.equal(run(MCP, { ...base, tool_name: "topodb/search_memories", tool_input: { query: "q" }, result_json: JSON.stringify({ content: [{ type: "text", text: JSON.stringify(search) }] }) }, { TOPODB_PLUGIN_DATA: dir }), "");
    let st = readState(dir, "conv-2");
    assert.equal(st.retrievals.length, 1); assert.equal(st.captured, undefined);
    assert.equal(run(MCP, { ...base, tool_name: "mcp__topodb__remember", tool_input: {}, result_json: JSON.stringify({ structuredContent: { memory_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV" } }) }, { TOPODB_PLUGIN_DATA: dir }), "");
    st = readState(dir, "conv-2"); assert.equal(st.captured, true);
    assert.ok(spooled(dir).some((e) => e.marker?.type === "memory_write"));
    assert.equal(run(MCP, { ...base, tool_name: "github/list_prs", tool_input: {}, result_json: "{}" }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(readState(dir, "conv-2").retrievals.length, 1);
    assert.equal(run(MCP, { ...base, tool_name: "topodb/search_memories", tool_input: {}, result_json: "{}" }, { TOPODB_PLUGIN_DATA: dir, TOPODB_RECORDING: "0" }), "");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("afterMCPExecution: another client's same-named tool is not mistaken for topodb's", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-mcp-"));
  try {
    const base = { hook_event_name: "afterMCPExecution", conversation_id: "conv-3", workspace_roots: [dir] };
    assert.equal(run(MCP, { ...base, tool_name: "github/remember", tool_input: {}, result_json: JSON.stringify({ structuredContent: { memory_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV" } }) }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(!readState(dir, "conv-3"));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
