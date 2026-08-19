import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { artifactEvent, newUlid, simpleDiff, warehouseDir, SPOOL_HARD_CAP } from "../warehouse-spool.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOOK = path.join(HERE, "..", "hooks", "warehouse-capture.js");

function runHook(payload, env) {
  return execFileSync(process.execPath, [HOOK], { input: JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 10000 }).toString();
}
function spooled(dataDir) {
  const dir = path.join(warehouseDir(dataDir, {}), "spool");
  let out = [];
  try { for (const f of readdirSync(dir)) out.push(...readFileSync(path.join(dir, f), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l))); } catch {}
  // One spool file per hook process; pids (hence readdir order) are not
  // monotonic on Windows, so order by event time, not filename.
  return out.sort((a, b) => a.ts - b.ts || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

test("newUlid is 26 chars, monotonic-ish, and simpleDiff marks lines", () => {
  const a = newUlid(1000), b = newUlid(1000);
  assert.equal(a.length, 26); assert.notEqual(a, b);
  const d = simpleDiff("a\nb\n", "a\nc\n");
  assert.match(d, /^--- old\n\+\+\+ new\n/); assert.match(d, /-b\n/); assert.match(d, /\+c\n/);
});

test("artifactEvent maps tools to artifact types and applies the hard cap", () => {
  const base = { sessionId: "s", scope: "01ARZ3NDEKTSV4RRFFQ69G5FAV", cwd: "/p", nowMs: 5 };
  const read = artifactEvent({ ...base, toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: { type: "text", file: { filePath: "/p/a.rs", content: "fn a(){}" } } });
  assert.equal(read.kind, "artifact"); assert.equal(read.artifact.type, "file_read"); assert.equal(read.artifact.locator, "/p/a.rs");
  assert.equal(read.artifact.content, "fn a(){}"); assert.equal(read.source.tool, "Read"); assert.equal(read.source.scope, base.scope);
  const bash = artifactEvent({ ...base, toolName: "Bash", toolInput: { command: "ls" }, toolResponse: { stdout: "x", stderr: "warn" } });
  assert.equal(bash.artifact.type, "command"); assert.equal(bash.artifact.locator, "ls"); assert.equal(bash.artifact.content, "x\n[stderr]\nwarn");
  const edit = artifactEvent({ ...base, toolName: "Edit", toolInput: { file_path: "/p/a.rs", old_string: "a", new_string: "b" }, toolResponse: {} });
  assert.equal(edit.artifact.type, "diff"); assert.match(edit.artifact.content, /-a\n\+b/);
  const write = artifactEvent({ ...base, toolName: "Write", toolInput: { file_path: "/p/n.rs", content: "new" }, toolResponse: {} });
  assert.equal(write.artifact.type, "diff"); assert.equal(write.artifact.content, "new");
  const grep = artifactEvent({ ...base, toolName: "Grep", toolInput: { pattern: "foo" }, toolResponse: { content: "a.rs:1:foo" } });
  assert.equal(grep.artifact.type, "tool_output"); assert.equal(grep.artifact.locator, "foo");
  const big = artifactEvent({ ...base, toolName: "Bash", toolInput: { command: "cat" }, toolResponse: { stdout: "z".repeat(SPOOL_HARD_CAP + 1) } });
  assert.equal(big.artifact.content, undefined); assert.match(big.artifact.hash, /^sha256:[0-9a-f]{64}$/); assert.equal(big.artifact.bytes, SPOOL_HARD_CAP + 1);
  assert.equal(artifactEvent({ ...base, toolName: "mcp__x__y", toolInput: {}, toolResponse: {} }), null);
  assert.equal(artifactEvent({ ...base, toolName: "Read", toolInput: {}, toolResponse: undefined }), null);
});

test("hook spools an event per tool call, honors kill switches, tags subagents", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-wh-"));
  try {
    const payload = { session_id: "sess-A", cwd: dataDir, hook_event_name: "PostToolUse", tool_name: "Read",
      tool_input: { file_path: "/x.rs" }, tool_response: { file: { content: "hello" } } };
    assert.equal(runHook(payload, { CLAUDE_PLUGIN_DATA: dataDir }), "");
    let evs = spooled(dataDir);
    assert.equal(evs.length, 1);
    assert.equal(evs[0].source.session, "sess-A"); assert.equal(evs[0].artifact.content, "hello");
    assert.equal(evs[0].source.harness, "claude-code"); assert.equal(evs[0].v, 1); assert.equal(evs[0].id.length, 26);
    runHook({ ...payload, agent_id: "ag1", agent_type: "Explore" }, { CLAUDE_PLUGIN_DATA: dataDir });
    evs = spooled(dataDir);
    assert.equal(evs.length, 2); assert.equal(evs[1].source.agent, "ag1"); assert.equal(evs[1].source.session, "sess-A");
    runHook(payload, { CLAUDE_PLUGIN_DATA: dataDir, TOPODB_RECORDING: "0" });
    runHook(payload, { CLAUDE_PLUGIN_DATA: dataDir, TOPODB_WAREHOUSE: "off" });
    assert.equal(spooled(dataDir).length, 2);
    runHook({ ...payload, tool_name: "mcp__plugin_topodb_topodb__search_memories" }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(spooled(dataDir).length, 2);
    // env override for the warehouse dir
    const alt = mkdtempSync(path.join(tmpdir(), "topodb-wh-alt-"));
    runHook(payload, { CLAUDE_PLUGIN_DATA: dataDir, TOPODB_WAREHOUSE_DIR: alt });
    assert.equal(readdirSync(path.join(alt, "spool")).length, 1);
    rmSync(alt, { recursive: true, force: true });
  } finally { rmSync(dataDir, { recursive: true, force: true }); }
});
