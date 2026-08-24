// test/extension-warehouse.test.ts — drives extension.ts's real
// session_start / tool_result / session_shutdown handlers and the topodb
// tool's memory_write branch, asserting what lands in the spool (spec §10).
// TopodbServer.prototype.call is monkey-patched so no topodb-mcp is spawned.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { TopodbServer } from "../src/server-handle.ts";
import registerExtension from "../src/extension.ts";

type Handler = (event: unknown, ctx: unknown) => Promise<unknown> | unknown;
type Tool = { execute: (...args: unknown[]) => Promise<unknown> };

function withEnv<T>(patch: Record<string, string | undefined>, fn: () => T): T {
  const saved: Record<string, string | undefined> = {};
  for (const k of Object.keys(patch)) { saved[k] = process.env[k]; if (patch[k] === undefined) delete process.env[k]; else process.env[k] = patch[k]; }
  try { return fn(); } finally { for (const k of Object.keys(saved)) { if (saved[k] === undefined) delete process.env[k]; else process.env[k] = saved[k]; } }
}

function harness(env: Record<string, string | undefined>, callImpl: (t: string, a: Record<string, unknown>) => unknown) {
  const handlers = new Map<string, Handler[]>();
  let tool: Tool | undefined;
  const pi = {
    on(event: string, h: Handler) { handlers.set(event, [...(handlers.get(event) ?? []), h]); },
    registerTool(def: Tool) { tool = def; },
  } as unknown as Parameters<typeof registerExtension>[0];
  const original = TopodbServer.prototype.call;
  TopodbServer.prototype.call = async (t: string, a: Record<string, unknown>) => callImpl(t, a);
  withEnv(env, () => registerExtension(pi));
  if (!tool) throw new Error("registerTool was never called");
  const fire = async (event: string, ev: unknown, ctx: unknown) => { for (const h of handlers.get(event) ?? []) await h(ev, ctx); };
  return { fire, tool, restore: () => { TopodbServer.prototype.call = original; } };
}

const ctxFor = (sessionId: string | undefined, cwd = "/w") => ({ cwd, sessionManager: { getSessionId: () => sessionId } });
const spooled = (dir: string) => {
  const s = path.join(dir, "spool");
  if (!existsSync(s)) return [] as Array<Record<string, any>>;
  return readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)));
};
const text = (t: string) => [{ type: "text", text: t }];
const MEM = "01MEMAAAAAAAAAAAAAAAAAAAAA";

test("a session lands start marker, artifacts, memory_write marker, and end marker tagged harness=pi", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "piwh-ext-"));
  const h = harness({ TOPODB_WAREHOUSE_DIR: dir, TOPODB_SCOPE: undefined, TOPODB_RECORD: undefined, TOPODB_WAREHOUSE: undefined, TOPODB_RECORDING: undefined },
    (t) => (t === "remember" ? { memory_id: MEM } : {}));
  try {
    const ctx = ctxFor("sess-1");
    await h.fire("session_start", { type: "session_start", reason: "startup" }, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c1", toolName: "read", input: { path: "/p/a.rs" }, content: text("fn a(){}"), details: undefined, isError: false }, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c2", toolName: "bash", input: { command: "ls" }, content: text("a\nb"), details: undefined, isError: false }, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c3", toolName: "edit", input: { path: "/p/a.rs", edits: [{ oldText: "a", newText: "b" }] }, content: text("ok"), details: { diff: "…" }, isError: false }, ctx);
    const res: any = await h.tool.execute("call-1", { tool: "remember", args: { content: "x" } }, undefined, undefined, ctx);
    assert.equal(res.details?.tool, "remember");
    assert.deepEqual(JSON.parse(res.content[0].text), { memory_id: MEM });
    await h.fire("session_shutdown", { type: "session_shutdown", reason: "quit" }, ctx);

    const evs = spooled(dir);
    assert.deepEqual(evs.map((e) => e.kind), ["marker", "artifact", "artifact", "artifact", "marker", "marker"]);
    assert.deepEqual(evs[0].marker, { type: "session_start", harness: "pi", session: "sess-1", scope: "shared" });
    assert.deepEqual(evs[1].source, { harness: "pi", session: "sess-1", scope: "shared", tool: "Read", cwd: "/w" });
    assert.deepEqual(evs[1].artifact, { type: "file_read", locator: "/p/a.rs", bytes: 8, content: "fn a(){}" });
    assert.equal(evs[2].artifact.type, "command");
    assert.equal(evs[2].artifact.locator, "ls");
    assert.equal(evs[3].artifact.type, "diff");
    assert.equal(evs[3].artifact.content, "--- old\n+++ new\n-a\n+b\n");
    assert.deepEqual(evs[4].marker, { type: "memory_write", harness: "pi", session: "sess-1", scope: "shared", node_ids: [MEM] });
    assert.deepEqual(evs[5].marker, { type: "session_end", harness: "pi", session: "sess-1", scope: "shared" });
    for (const e of evs) { assert.match(e.id, /^[0-9A-HJKMNP-TV-Z]{26}$/); assert.equal(e.v, 1); assert.equal(e.host, ""); }
    assert.equal(readdirSync(path.join(dir, "spool"))[0], `sess-1-${process.pid}.jsonl`);
  } finally {
    h.restore();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scope comes from TOPODB_SCOPE, and the warehouse dir defaults to <TOPODB_DB>.warehouse", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "piwh-db-"));
  const db = path.join(root, "team.redb");
  const h = harness({ TOPODB_WAREHOUSE_DIR: undefined, TOPODB_DB: db, TOPODB_SCOPE: "01SCOPEAAAAAAAAAAAAAAAAAAA", TOPODB_RECORD: undefined, TOPODB_WAREHOUSE: undefined, TOPODB_RECORDING: undefined }, () => ({}));
  try {
    await h.fire("session_start", { type: "session_start", reason: "startup" }, ctxFor("s2"));
    const evs = spooled(path.join(root, "team.warehouse"));
    assert.equal(evs.length, 1);
    assert.equal(evs[0].marker.scope, "01SCOPEAAAAAAAAAAAAAAAAAAA");
  } finally {
    h.restore();
    rmSync(root, { recursive: true, force: true });
  }
});

test("nothing is spooled for errors, ls, the topodb tool, create_memory with no id, a missing session id, or when disabled", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "piwh-drop-"));
  const h = harness({ TOPODB_WAREHOUSE_DIR: dir, TOPODB_RECORD: undefined, TOPODB_WAREHOUSE: undefined, TOPODB_RECORDING: undefined }, () => ({ ok: true }));
  try {
    const ctx = ctxFor("s3");
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "bash", input: { command: "x" }, content: text("boom"), details: undefined, isError: true }, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "ls", input: { path: "." }, content: text("a"), details: undefined, isError: false }, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "topodb", input: { tool: "remember" }, content: text("{}"), details: undefined, isError: false }, ctx);
    await h.tool.execute("call-2", { tool: "create_memory", args: { content: "x" } }, undefined, undefined, ctx);
    await h.tool.execute("call-3", { tool: "search_memories", args: { query: "x" } }, undefined, undefined, ctx);
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "read", input: { path: "/p" }, content: text("t"), details: undefined, isError: false }, ctxFor(undefined));
    await h.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "read", input: { path: "/p" }, content: text("t"), details: undefined, isError: false }, {});
    await h.fire("session_start", { type: "session_start", reason: "startup" }, undefined);
    assert.deepEqual(spooled(dir), []);
  } finally {
    h.restore();
    rmSync(dir, { recursive: true, force: true });
  }
  for (const env of [{ TOPODB_RECORD: "0" }, { TOPODB_WAREHOUSE: "off" }, { TOPODB_RECORDING: "0" }]) {
    const d = mkdtempSync(path.join(tmpdir(), "piwh-off-"));
    const g = harness({ TOPODB_WAREHOUSE_DIR: d, TOPODB_RECORD: undefined, TOPODB_WAREHOUSE: undefined, TOPODB_RECORDING: undefined, ...env }, () => ({ memory_id: MEM }));
    try {
      const ctx = ctxFor("s4");
      await g.fire("session_start", { type: "session_start", reason: "startup" }, ctx);
      await g.fire("tool_result", { type: "tool_result", toolCallId: "c", toolName: "read", input: { path: "/p" }, content: text("t"), details: undefined, isError: false }, ctx);
      await g.tool.execute("call-4", { tool: "remember", args: {} }, undefined, undefined, ctx);
      await g.fire("session_shutdown", { type: "session_shutdown", reason: "quit" }, ctx);
      assert.deepEqual(spooled(d), [], JSON.stringify(env));
    } finally {
      g.restore();
      rmSync(d, { recursive: true, force: true });
    }
  }
});

test("a spool write failure is logged, never thrown, and the tool result is still returned", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "piwh-fail-"));
  const blocker = path.join(root, "not-a-dir");
  // A regular FILE where the warehouse dir should be makes mkdirSync(spool) throw ENOTDIR/EEXIST.
  writeFileSync(blocker, "x");
  const errors: string[] = [];
  const origErr = console.error;
  console.error = (m: unknown) => { errors.push(String(m)); };
  const h = harness({ TOPODB_WAREHOUSE_DIR: blocker, TOPODB_RECORD: undefined, TOPODB_WAREHOUSE: undefined, TOPODB_RECORDING: undefined }, () => ({ memory_id: MEM }));
  try {
    const ctx = ctxFor("s5");
    await h.fire("session_start", { type: "session_start", reason: "startup" }, ctx);
    const res: any = await h.tool.execute("call-5", { tool: "remember", args: {} }, undefined, undefined, ctx);
    assert.deepEqual(JSON.parse(res.content[0].text), { memory_id: MEM });
    assert.ok(errors.some((e) => e.startsWith("topodb warehouse:")), `expected a topodb warehouse: log line, got ${JSON.stringify(errors)}`);
  } finally {
    console.error = origErr;
    h.restore();
    rmSync(root, { recursive: true, force: true });
  }
});
