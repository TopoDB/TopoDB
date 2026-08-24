// test/warehouse-parity.test.ts — the "format shared" guarantee (spec §4):
// src/warehouse.ts must produce byte-identical events to plugins/core for the
// same canonical input, modulo the random ULID `id`. Runs in-repo only; the
// published package never imports plugins/core.
import { test } from "node:test";
import assert from "node:assert/strict";
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
