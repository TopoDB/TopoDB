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
const fixture = (n) => JSON.parse(readFileSync(path.join(HERE, "fixtures", n), "utf8"));
const run = (payload, env) => execFileSync(process.execPath, [CAP], { input: typeof payload === "string" ? payload : JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();
const spooled = (dir) => { const s = path.join(dir, "memory.warehouse", "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)) : []; };

test("documented PostToolUse shapes spool with harness=codex; apply_patch arrives normalized as Edit/Write", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-cap-"));
  try {
    for (const n of ["post-tool-use.bash.json", "post-tool-use.read.json", "post-tool-use.apply-patch-edit.json", "post-tool-use.apply-patch-write.json"]) {
      assert.equal(run({ ...fixture(n), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "", `${n}: nothing on stdout`);
    }
    const evs = spooled(dir).filter((e) => e.kind === "artifact");
    assert.equal(evs.length, 4);
    for (const e of evs) {
      assert.equal(e.source.harness, "codex");
      assert.equal(e.source.session, "codex-sess-0001");
    }
    const byTool = Object.fromEntries(evs.map((e) => [e.source.tool, e]));
    assert.equal(byTool.Bash.artifact.type, "command");
    assert.equal(byTool.Bash.artifact.locator, "ls");
    assert.equal(byTool.Bash.artifact.content, "a\n");
    assert.equal(byTool.Read.artifact.type, "file_read");
    assert.equal(byTool.Read.artifact.content, "fn main() {}\n");
    assert.equal(byTool.Edit.artifact.type, "diff");
    assert.match(byTool.Edit.artifact.content, /-let x = 1;/);
    assert.match(byTool.Edit.artifact.content, /\+let x = 2;/);
    assert.equal(byTool.Write.artifact.type, "diff");
    assert.equal(byTool.Write.artifact.content, "written by apply_patch\n");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("mcp__topodb__* matches: search records a retrieval, remember marks captured + memory_write marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-cap-"));
  try {
    assert.equal(run({ ...fixture("post-tool-use.mcp-search.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    let st = readState(dir, "codex-sess-0001");
    assert.equal(st.retrievals.length, 1);
    assert.equal(st.retrievals[0].returned[0].id, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert.equal(st.captured, undefined);
    assert.equal(run({ ...fixture("post-tool-use.mcp-remember.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    st = readState(dir, "codex-sess-0001");
    assert.equal(st.captured, true);
    assert.ok(spooled(dir).some((e) => e.marker?.type === "memory_write"));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("mcp__topodb__remember: an explicit tool_input.scope lands on the memory_write marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-cap-"));
  try {
    const payload = fixture("post-tool-use.mcp-remember.json");
    payload.tool_input = { content: "x", scope: "01ARZ3NDEKTSV4RRFFQ69G5FAV" };
    assert.equal(run({ ...payload, cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    const marker = spooled(dir).find((e) => e.marker?.type === "memory_write");
    assert.ok(marker, "expected a memory_write marker");
    assert.equal(marker.marker.scope, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("a foreign server's same-named tool is not mistaken for topodb's", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-cap-"));
  try {
    assert.equal(run({ ...fixture("post-tool-use.mcp-foreign.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(!readState(dir, "codex-sess-0002"));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("kill switches and garbage stdin: TOPODB_WAREHOUSE=0 / TOPODB_RECORDING=0 suppress everything, exit 0 always", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-cap-"));
  try {
    assert.equal(run({ ...fixture("post-tool-use.bash.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_WAREHOUSE: "0" }), "");
    assert.equal(run({ ...fixture("post-tool-use.bash.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_RECORDING: "0" }), "");
    assert.equal(run({ ...fixture("post-tool-use.mcp-search.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_RECORDING: "0" }), "");
    assert.equal(run(readFileSync(path.join(HERE, "fixtures", "garbage.txt"), "utf8"), { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
    assert.ok(!readState(dir, "codex-sess-0001"));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
