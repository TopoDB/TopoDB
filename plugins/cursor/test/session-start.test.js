import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { hookContext } from "../hooks/_env.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOOK = path.join(HERE, "..", "hooks", "session-start.js");
const run = (payload, env) => execFileSync(process.execPath, [HOOK], { input: typeof payload === "string" ? payload : JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();

test("hookContext resolves ids and dirs from Cursor's payload and env", () => {
  const c = hookContext({ session_id: "S", conversation_id: "C", workspace_roots: ["/w"] }, { TOPODB_PLUGIN_DATA: "/d" });
  assert.deepEqual(c, { dataDir: "/d", projectDir: "/w", sessionId: "S" });
  assert.equal(hookContext({ conversation_id: "C" }, { TOPODB_PLUGIN_DATA: "/d", TOPODB_SESSION_ID: "E", CURSOR_PROJECT_DIR: "/p" }).sessionId, "E");
  assert.equal(hookContext({ conversation_id: "C" }, { TOPODB_PLUGIN_DATA: "/d" }).sessionId, "C");
});
test("no daemon: emits only the env frame, writes a session_start marker, exits 0", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-ss-"));
  try {
    const out = run({ hook_event_name: "sessionStart", session_id: "sess-1", conversation_id: "sess-1", workspace_roots: [dir], is_background_agent: false }, { TOPODB_PLUGIN_DATA: dir });
    assert.deepEqual(JSON.parse(out), { env: { TOPODB_SESSION_ID: "sess-1" } });
    const spool = path.join(dir, "memory.warehouse", "spool");
    const evs = readdirSync(spool).flatMap((f) => readFileSync(path.join(spool, f), "utf8").split("\n").filter(Boolean).map(JSON.parse));
    assert.equal(evs[0].marker.type, "session_start"); assert.equal(evs[0].marker.harness, "cursor");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("background agents and garbage stdin produce no output", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-ss-"));
  try {
    assert.equal(run({ session_id: "b", is_background_agent: true, workspace_roots: [dir] }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(run("not json", { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("TOPODB_WAREHOUSE=0 keeps the env frame but writes no spool marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-ss-"));
  try {
    const out = run({ session_id: "k1", workspace_roots: [dir] }, { TOPODB_PLUGIN_DATA: dir, TOPODB_WAREHOUSE: "0" });
    assert.deepEqual(JSON.parse(out), { env: { TOPODB_SESSION_ID: "k1" } });
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("TOPODB_RECORDING=0 keeps the env frame but writes no spool marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-ss-"));
  try {
    const out = run({ session_id: "k2", workspace_roots: [dir] }, { TOPODB_PLUGIN_DATA: dir, TOPODB_RECORDING: "0" });
    assert.deepEqual(JSON.parse(out), { env: { TOPODB_SESSION_ID: "k2" } });
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("TOPODB_HOOK_DEBUG dumps the raw payload", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cur-ss-"));
  try {
    run({ session_id: "d", workspace_roots: [dir] }, { TOPODB_PLUGIN_DATA: dir, TOPODB_HOOK_DEBUG: "1" });
    assert.ok(existsSync(path.join(dir, "episodes", "debug-sessionStart.json")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
