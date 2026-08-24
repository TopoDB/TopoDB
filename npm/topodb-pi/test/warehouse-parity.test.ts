// test/warehouse-parity.test.ts — the "format shared" guarantee (spec §4):
// src/warehouse.ts must produce byte-identical events to plugins/core for the
// same canonical input, modulo the random ULID `id`. Runs in-repo only; the
// published package never imports plugins/core.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import * as core from "../../../plugins/core/warehouse-spool.js";
import * as pi from "../src/warehouse.ts";

const strip = (e: unknown) => { const { id: _id, ...rest } = e as { id: string }; return JSON.stringify(rest); };
const base = { sessionId: "s1", scope: "01SCOPEAAAAAAAAAAAAAAAAAAA", cwd: "/w", agent: undefined, harness: "pi", nowMs: 1_700_000_000_000 };

const CASES = [
  { toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: "fn a(){}\n" },
  { toolName: "Bash", toolInput: { command: "ls -la" }, toolResponse: "a\nb\n" },
  { toolName: "Edit", toolInput: { file_path: "/p/a.rs", old_string: "a\nb", new_string: "a\nB" }, toolResponse: "ok" },
  { toolName: "MultiEdit", toolInput: { file_path: "/p/a.rs", edits: [{ old_string: "a", new_string: "b" }, { old_string: "c\n", new_string: "d\n" }] }, toolResponse: "ok" },
  { toolName: "Write", toolInput: { file_path: "/p/n.rs", content: "new\n" }, toolResponse: "wrote" },
  { toolName: "Grep", toolInput: { pattern: "foo" }, toolResponse: "a.rs:1:foo" },
  { toolName: "Glob", toolInput: { pattern: "**/*.rs" }, toolResponse: "a.rs\nb.rs" },
  { toolName: "Bash", toolInput: { command: "cat big" }, toolResponse: "z".repeat(pi.SPOOL_HARD_CAP + 7) },
  { toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: [{ type: "text", text: "block" }] },
  { toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: { file: { content: "obj" } } },
  { toolName: "Bash", toolInput: { command: "x" }, toolResponse: { stdout: "out", stderr: "err" } },
];

test("artifactEvent parity with plugins/core for every canonical tool", () => {
  for (const c of CASES) {
    const a = core.artifactEvent({ ...c, ...base });
    const b = pi.artifactEvent({ ...c, ...base } as Parameters<typeof pi.artifactEvent>[0]);
    assert.ok(a && b, `${c.toolName}: both implementations must produce an event`);
    assert.equal(strip(b), strip(a), `${c.toolName}: event JSON differs from plugins/core`);
  }
});

test("markerEvent parity with plugins/core", () => {
  const args = { type: "memory_write", sessionId: "s1", scope: "shared", nodeIds: ["01MEMAAAAAAAAAAAAAAAAAAAAA"], harness: "pi", nowMs: 1_700_000_000_000 };
  assert.equal(strip(pi.markerEvent(args)), strip(core.markerEvent(args)));
  const plain = { type: "session_start", sessionId: "s1", scope: "shared", harness: "pi", nowMs: 1_700_000_000_000 };
  assert.equal(strip(pi.markerEvent(plain)), strip(core.markerEvent(plain)));
});

test("spool file naming parity with plugins/core", () => {
  // core takes the plugin data dir and appends memory.warehouse; ours takes the warehouse dir itself.
  const c = core.spoolPath("/data", "sess/1", {});
  const p = pi.spoolPath("/data/memory.warehouse", "sess/1");
  assert.equal(p, c);
});

test("ULID alphabet parity with plugins/core", () => {
  const ts = 1_700_000_000_000;
  assert.equal(pi.newUlid(ts).slice(0, 10), core.newUlid(ts).slice(0, 10));
});

const tomlIo = (files: Record<string, string>) => {
  const norm = (p: string) => path.normalize(p);
  const map = new Map(Object.entries(files).map(([k, v]) => [norm(k), v]));
  return { existsFile: (p: string) => map.has(norm(p)), readFile: (p: string) => { if (!map.has(norm(p))) throw new Error("ENOENT"); return map.get(norm(p))!; } };
};

test("parseWarehouseToml parity with plugins/core", () => {
  for (const t of ["", "[warehouse]\nenabled = false # x\npath = \"wh\"\n", "[warehouse]\npath = \"a # b\"\n", "[ warehouse ]\r\nenabled=true\r\n", "[warehouse.sub]\npath = \"no\"\n", "[[warehouse]]\npath = \"arr\"\n", "[warehouse]\npath = ''\n", "[warehouse]\npath = 'x' # c\n"]) {
    assert.deepEqual(pi.parseWarehouseToml(t), core.parseWarehouseToml(t), JSON.stringify(t));
  }
});

test("resolveWarehouse parity with plugins/core", () => {
  const homeKey = process.platform === "win32" ? "USERPROFILE" : "HOME";
  const cases: Array<[string, Record<string, string>, Record<string, string>]> = [
    ["/p/sub/memory.redb", {}, {}],
    ["/p/sub/memory.redb", { TOPODB_WAREHOUSE_DIR: "/w " }, {}],
    ["/p/sub/memory.redb", {}, { "/p/.topodb.toml": "[warehouse]\npath = \"wh\"\n" }],
    ["/p/sub/memory.redb", { [homeKey]: "" }, { "/p/.topodb.toml": "[warehouse]\npath = \"~/wh\"\n" }],
    ["/p/sub/memory.redb", { [homeKey]: "/home/u" }, { "/p/.topodb.toml": "[warehouse]\npath = \"~/wh\"\n" }],
    ["/p/sub/memory.redb", {}, { "/p/.topodb.toml": "[warehouse]\npath = \"~/wh\"\n" }], // unset HOME: literal ~/ kept
    ["/p/sub/memory.redb", { TOPODB_WAREHOUSE: "1", TOPODB_WAREHOUSE_DIR: "/env" }, { "/p/.topodb.toml": "[warehouse]\nenabled = false\n" }],
    [".topodb/memory.redb", {}, { ".topodb.toml": "[warehouse]\npath = \"wh\"\n" }],
    ["db", { TOPODB_WAREHOUSE: " off " }, {}],
  ];
  for (const [db, env, files] of cases) {
    assert.deepEqual(pi.resolveWarehouse(db, env, tomlIo(files)), core.resolveWarehouse(db, env, tomlIo(files)), `${db} ${JSON.stringify(env)}`);
  }
});
