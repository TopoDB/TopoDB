import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { warehouseDir } from "../warehouse-spool.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOOKS = path.join(HERE, "..", "hooks");
function runHook(script, payload, env) {
  return execFileSync(process.execPath, [path.join(HOOKS, script)], { input: JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 10000 }).toString();
}
function markers(dataDir) {
  const dir = path.join(warehouseDir(dataDir, {}), "spool");
  const out = [];
  try { for (const f of readdirSync(dir)) for (const l of readFileSync(path.join(dir, f), "utf8").split("\n")) if (l) { const e = JSON.parse(l); if (e.kind === "marker") out.push(e.marker); } } catch {}
  return out;
}

test("session-start/end and mark-captured write markers into the spool", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-mk-"));
  try {
    const env = { CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: dataDir };
    runHook("session-start.js", { session_id: "S", source: "startup", cwd: dataDir }, env); // no broker: still marks
    runHook("mark-captured.js", { session_id: "S", tool_name: "mcp__plugin_topodb_topodb__remember", tool_input: {}, tool_response: { memory_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", deduplicated: false } }, env);
    runHook("mark-captured.js", { session_id: "S", tool_name: "mcp__plugin_topodb_topodb__create_memory", tool_input: {}, tool_response: { content: [{ type: "text", text: JSON.stringify({ id: "01ARZ3NDEKTSV4RRFFQ69G5FAW" }) }] } }, env);
    runHook("session-end.js", { session_id: "S", cwd: dataDir }, env);
    const m = markers(dataDir);
    assert.deepEqual(m.map((x) => x.type), ["session_start", "memory_write", "memory_write", "session_end"]);
    assert.deepEqual(m[1].node_ids, ["01ARZ3NDEKTSV4RRFFQ69G5FAV"]);
    assert.deepEqual(m[2].node_ids, ["01ARZ3NDEKTSV4RRFFQ69G5FAW"]);
    assert.ok(m.every((x) => x.session === "S" && x.harness === "claude-code" && typeof x.scope === "string" && x.scope.length === 26));
    // kill switch
    runHook("session-end.js", { session_id: "S2", cwd: dataDir }, { ...env, TOPODB_WAREHOUSE: "0" });
    assert.equal(markers(dataDir).filter((x) => x.session === "S2").length, 0);
    // subagent session-start: no marker (main sessions only)
    runHook("session-start.js", { session_id: "S3", source: "startup", cwd: dataDir, agent_type: "Explore" }, env);
    assert.equal(markers(dataDir).filter((x) => x.session === "S3").length, 0);
  } finally { rmSync(dataDir, { recursive: true, force: true }); }
});
