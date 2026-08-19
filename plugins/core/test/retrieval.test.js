import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { recordRetrieval, recordMemoryWrite, bareToolName, RETRIEVAL_TOOLS } from "../hooks/retrieval.js";
import { readState } from "../recorder.js";
import { sessionScopes } from "../server-args.js";
import { warehouseDir } from "../warehouse-spool.js";

test("bareToolName strips client prefixes", () => {
  assert.equal(bareToolName("mcp__plugin_topodb_topodb__search_memories"), "search_memories");
  assert.equal(bareToolName("topodb/traverse"), "traverse");
  assert.equal(bareToolName("topodb:recent_memories"), "recent_memories");
  assert.equal(bareToolName("remember"), "remember");
  assert.ok(RETRIEVAL_TOOLS.includes("search_memories"));
});
test("recordRetrieval appends a record; non-retrieval tools are ignored", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "retr-"));
  try {
    const result = { content: [{ type: "text", text: JSON.stringify({ hits: [{ node: { id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", props: { content: "hello" } }, score: 0.9 }] }) }] };
    assert.equal(recordRetrieval({ dataDir: dir, sessionId: "s1", toolName: "MCP:topodb/search_memories", toolInput: { query: "q" }, toolResult: result }), true);
    const st = readState(dir, "s1");
    assert.equal(st.retrievals.length, 1);
    assert.equal(st.contents["01ARZ3NDEKTSV4RRFFQ69G5FAV"], "hello");
    assert.equal(recordRetrieval({ dataDir: dir, sessionId: "s1", toolName: "remember", toolInput: {}, toolResult: {} }), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("recordMemoryWrite marks captured and spools a memory_write marker with the harness", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "retr-"));
  try {
    const ids = recordMemoryWrite({ dataDir: dir, env: {}, projectDir: dir, sessionId: "s2", harness: "cursor",
      toolResult: { structuredContent: { memory_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV" } } });
    assert.deepEqual(ids, ["01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
    assert.equal(readState(dir, "s2").captured, true);
    const spool = path.join(warehouseDir(dir, {}), "spool");
    const evs = readdirSync(spool).flatMap((f) => readFileSync(path.join(spool, f), "utf8").split("\n").filter(Boolean).map(JSON.parse));
    assert.equal(evs.length, 1); assert.equal(evs[0].marker.type, "memory_write"); assert.equal(evs[0].marker.harness, "cursor");
    assert.equal(evs[0].marker.scope, sessionScopes({ projectDir: dir }).scope);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
