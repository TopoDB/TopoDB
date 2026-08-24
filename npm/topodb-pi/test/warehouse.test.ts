// test/warehouse.test.ts — pure helpers in src/warehouse.ts (spec §3, §5, §6, §8).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import {
  HARNESS, SPOOL_HARD_CAP, warehouseDisabled, warehouseDirForDb, spoolPath, appendSpool, newUlid,
  simpleDiff, fromPiToolResult, artifactEvent, markerEvent, memoryWriteIds,
} from "../src/warehouse.ts";

test("harness label is pi", () => {
  assert.equal(HARNESS, "pi");
});

test("off switches: TOPODB_RECORD=0, TOPODB_RECORDING, TOPODB_WAREHOUSE (0/off, case-insensitive)", () => {
  assert.equal(warehouseDisabled({}), false);
  assert.equal(warehouseDisabled({ TOPODB_RECORD: "0" }), true);
  assert.equal(warehouseDisabled({ TOPODB_RECORD: "1" }), false);
  assert.equal(warehouseDisabled({ TOPODB_RECORDING: "0" }), true);
  assert.equal(warehouseDisabled({ TOPODB_RECORDING: "OFF" }), true);
  assert.equal(warehouseDisabled({ TOPODB_WAREHOUSE: "off" }), true);
  assert.equal(warehouseDisabled({ TOPODB_WAREHOUSE: "1" }), false);
});

test("warehouse dir follows the Rust <db>.warehouse rule, TOPODB_WAREHOUSE_DIR overrides", () => {
  assert.equal(warehouseDirForDb(".topodb/memory.redb", {}), ".topodb/memory.warehouse");
  assert.equal(warehouseDirForDb("/x/y/team.redb", {}), "/x/y/team.warehouse");
  assert.equal(warehouseDirForDb("/x/y/db", {}), "/x/y/db.warehouse");
  assert.equal(warehouseDirForDb("/x/y/a.b.c", {}), "/x/y/a.b.warehouse");
  assert.equal(warehouseDirForDb("/x/y/team.redb", { TOPODB_WAREHOUSE_DIR: "/w" }), "/w");
  assert.equal(warehouseDirForDb("/x/y/team.redb", { TOPODB_WAREHOUSE_DIR: "   " }), "/x/y/team.warehouse");
});

test("spoolPath sanitizes the session id and appendSpool writes one JSON line per event", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "piwh-"));
  try {
    const p = spoolPath(dir, "a/b c");
    assert.equal(path.dirname(p), path.join(dir, "spool"));
    assert.match(path.basename(p), new RegExp(`^a_b_c-${process.pid}\\.jsonl$`));
    appendSpool(dir, "s1", { a: 1 });
    appendSpool(dir, "s1", { b: 2 });
    const files = readdirSync(path.join(dir, "spool"));
    assert.equal(files.length, 1);
    const lines = readFileSync(path.join(dir, "spool", files[0]), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
    assert.deepEqual(lines, [{ a: 1 }, { b: 2 }]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("newUlid is 26 Crockford chars and time-ordered", () => {
  const a = newUlid(1_700_000_000_000);
  const b = newUlid(1_700_000_000_001);
  assert.match(a, /^[0-9A-HJKMNP-TV-Z]{26}$/);
  assert.ok(a.slice(0, 10) < b.slice(0, 10));
});

test("simpleDiff emits the core --- old/+++ new hunk shape", () => {
  assert.equal(simpleDiff("a\nb\nc\n", "a\nB\nc\n"), "--- old\n+++ new\n-b\n+B\n");
  assert.equal(simpleDiff(undefined, "x"), "--- old\n+++ new\n+x\n");
});

const text = (t: string) => [{ type: "text", text: t }];

test("fromPiToolResult maps bash/read/edit/write/grep/find onto the canonical vocabulary", () => {
  assert.deepEqual(fromPiToolResult({ toolName: "bash", input: { command: "ls", timeout: 5 }, content: text("a\nb"), isError: false }),
    { toolName: "Bash", toolInput: { command: "ls" }, toolResponse: "a\nb" });
  assert.deepEqual(fromPiToolResult({ toolName: "read", input: { path: "/p/a.rs", offset: 1 }, content: text("fn a(){}"), isError: false }),
    { toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: "fn a(){}" });
  assert.deepEqual(fromPiToolResult({ toolName: "edit", input: { path: "/p/a.rs", edits: [{ oldText: "a", newText: "b" }, { oldText: "c", newText: "d" }] }, content: text("ok"), isError: false }),
    { toolName: "MultiEdit", toolInput: { file_path: "/p/a.rs", edits: [{ old_string: "a", new_string: "b" }, { old_string: "c", new_string: "d" }] }, toolResponse: "ok" });
  assert.deepEqual(fromPiToolResult({ toolName: "write", input: { path: "/p/n.rs", content: "new" }, content: text("wrote"), isError: false }),
    { toolName: "Write", toolInput: { file_path: "/p/n.rs", content: "new" }, toolResponse: "wrote" });
  assert.deepEqual(fromPiToolResult({ toolName: "grep", input: { pattern: "foo", path: "src" }, content: text("a.rs:1:foo"), isError: false }),
    { toolName: "Grep", toolInput: { pattern: "foo" }, toolResponse: "a.rs:1:foo" });
  assert.deepEqual(fromPiToolResult({ toolName: "find", input: { pattern: "**/*.rs" }, content: text("a.rs\nb.rs"), isError: false }),
    { toolName: "Glob", toolInput: { pattern: "**/*.rs" }, toolResponse: "a.rs\nb.rs" });
});

test("fromPiToolResult drops errors, ls, custom/MCP tools, the topodb tool, image reads, and empty edits", () => {
  assert.equal(fromPiToolResult({ toolName: "bash", input: { command: "ls" }, content: text("boom"), isError: true }), null);
  assert.equal(fromPiToolResult({ toolName: "ls", input: { path: "." }, content: text("a\nb"), isError: false }), null);
  assert.equal(fromPiToolResult({ toolName: "topodb", input: { tool: "remember" }, content: text("{}"), isError: false }), null);
  assert.equal(fromPiToolResult({ toolName: "mcp__github__search", input: {}, content: text("x"), isError: false }), null);
  assert.equal(fromPiToolResult({ toolName: "read", input: { path: "/p/img.png" }, content: [{ type: "image", data: "…", mimeType: "image/png" }], isError: false }), null);
  assert.equal(fromPiToolResult({ toolName: "edit", input: { path: "/p/a.rs", edits: [] }, content: text("ok"), isError: false }), null);
  assert.equal(fromPiToolResult({ toolName: "edit", input: { path: "/p/a.rs", edits: [{ oldText: 1 }] }, content: text("ok"), isError: false }), null);
  // undefined input is tolerated (never throws)
  assert.deepEqual(fromPiToolResult({ toolName: "bash", content: text("x"), isError: false }), { toolName: "Bash", toolInput: { command: undefined }, toolResponse: "x" });
  // Write/MultiEdit take their text from the input, so a result with no text block is still landed.
  assert.deepEqual(fromPiToolResult({ toolName: "write", input: { path: "/p/n.rs", content: "new" }, content: [], isError: false }),
    { toolName: "Write", toolInput: { file_path: "/p/n.rs", content: "new" }, toolResponse: undefined });
  assert.deepEqual(fromPiToolResult({ toolName: "edit", input: { path: "/p/a.rs", edits: [{ oldText: "a", newText: "b" }] }, content: [], isError: false }),
    { toolName: "MultiEdit", toolInput: { file_path: "/p/a.rs", edits: [{ old_string: "a", new_string: "b" }] }, toolResponse: undefined });
});

test("artifactEvent tags source.harness=pi, computes bytes, and hashes above the hard cap", () => {
  const base = { sessionId: "s1", scope: "shared", cwd: "/w", harness: HARNESS, nowMs: 1_700_000_000_000 };
  const ev = artifactEvent({ ...base, toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: "héllo" })!;
  assert.equal(ev.kind, "artifact");
  assert.equal(ev.v, 1);
  assert.equal(ev.ts, 1_700_000_000_000);
  assert.deepEqual(ev.source, { harness: "pi", session: "s1", scope: "shared", tool: "Read", cwd: "/w" });
  assert.deepEqual(ev.artifact, { type: "file_read", locator: "/p/a.rs", bytes: 6, content: "héllo" });
  const big = "x".repeat(SPOOL_HARD_CAP + 1);
  const bigEv = artifactEvent({ ...base, toolName: "Bash", toolInput: { command: "cat big" }, toolResponse: big })!;
  assert.equal(bigEv.artifact.content, undefined);
  assert.match(String(bigEv.artifact.hash), /^sha256:[0-9a-f]{64}$/);
  assert.equal(artifactEvent({ ...base, toolName: "Task", toolInput: {}, toolResponse: "x" }), null);
  assert.equal(artifactEvent({ ...base, toolName: "Read", toolInput: {}, toolResponse: undefined }), null);
});

test("markerEvent carries node_ids only when present", () => {
  const m = markerEvent({ type: "session_start", sessionId: "s1", scope: "shared", harness: HARNESS, nowMs: 1 });
  assert.deepEqual(m.marker, { type: "session_start", harness: "pi", session: "s1", scope: "shared" });
  const w = markerEvent({ type: "memory_write", sessionId: "s1", scope: "shared", nodeIds: ["01MEMAAAAAAAAAAAAAAAAAAAAA"], harness: HARNESS, nowMs: 1 });
  assert.deepEqual(w.marker.node_ids, ["01MEMAAAAAAAAAAAAAAAAAAAAA"]);
});

test("memoryWriteIds finds 26-char ids in object, text-block, and string results; dedupes", () => {
  const id = "01MEMAAAAAAAAAAAAAAAAAAAAA";
  assert.deepEqual(memoryWriteIds({ memory_id: id, id }), [id]);
  assert.deepEqual(memoryWriteIds({ node: { id } }), [id]);
  assert.deepEqual(memoryWriteIds({ memory: { id } }), [id]);
  assert.deepEqual(memoryWriteIds([{ type: "text", text: JSON.stringify({ id }) }]), [id]);
  assert.deepEqual(memoryWriteIds(JSON.stringify({ memory_id: id })), [id]);
  assert.deepEqual(memoryWriteIds({ id: "short" }), []);
  assert.deepEqual(memoryWriteIds("not json"), []);
  assert.deepEqual(memoryWriteIds(undefined), []);
});
