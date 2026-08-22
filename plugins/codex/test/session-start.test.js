import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { hookContext, HARNESS } from "../hooks/_env.js";
import { ADDITIONAL_CONTEXT_LIMIT, buildOutput } from "../hooks/session-start.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOOK = path.join(HERE, "..", "hooks", "session-start.js");
const fixture = (n) => JSON.parse(readFileSync(path.join(HERE, "fixtures", n), "utf8"));
const run = (payload, env) => execFileSync(process.execPath, [HOOK], { input: typeof payload === "string" ? payload : JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();
const spooled = (dir) => { const s = path.join(dir, "memory.warehouse", "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)) : []; };

test("hookContext normalizes Codex's env frame: data-dir chain, project dir, session id", () => {
  assert.equal(HARNESS, "codex");
  assert.deepEqual(hookContext({ session_id: "S", cwd: "/w" }, { TOPODB_PLUGIN_DATA: "/d" }), { dataDir: "/d", projectDir: "/w", sessionId: "S" });
  // Codex sets PLUGIN_DATA plus CLAUDE_PLUGIN_DATA "for compatibility"; _env.js normalizes the pair ONCE.
  assert.equal(hookContext({ session_id: "S", cwd: "/w" }, { CLAUDE_PLUGIN_DATA: "/cd" }).dataDir, "/cd");
  assert.equal(hookContext({ session_id: "S", cwd: "/w" }, { PLUGIN_DATA: "/pd" }).dataDir, "/pd");
  assert.equal(hookContext({ session_id: "S", cwd: "/w" }, { TOPODB_PLUGIN_DATA: "/d", CLAUDE_PROJECT_DIR: "/p" }).projectDir, "/p");
  assert.equal(hookContext({ session_id: "S" }, { TOPODB_PLUGIN_DATA: "/d", TOPODB_SESSION_ID: "E" }).sessionId, "E");
});

test("every SessionStart source — startup, resume, clear, AND compact — runs the same recall path", () => {
  // compact is the post-compaction re-inject (Codex-only): the script must NOT
  // gate on source the way claude-code's startup|clear-only hook does.
  for (const name of ["session-start.startup.json", "session-start.resume.json", "session-start.clear.json", "session-start.compact.json"]) {
    const dir = mkdtempSync(path.join(tmpdir(), "cdx-ss-"));
    try {
      const out = run({ ...fixture(name), cwd: dir }, { TOPODB_PLUGIN_DATA: dir });
      assert.equal(out, "", `${name}: no daemon yet → inject nothing, print nothing`);
      const evs = spooled(dir);
      assert.equal(evs[0].marker.type, "session_start", name);
      assert.equal(evs[0].marker.harness, "codex", name);
    } finally { rmSync(dir, { recursive: true, force: true }); }
  }
});

test("background agents and garbage stdin produce no output and no spool", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-ss-"));
  try {
    assert.equal(run({ ...fixture("session-start.background.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.equal(run(readFileSync(path.join(HERE, "fixtures", "garbage.txt"), "utf8"), { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("TOPODB_WAREHOUSE=0 writes no spool marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-ss-"));
  try {
    assert.equal(run({ ...fixture("session-start.startup.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_WAREHOUSE: "0" }), "");
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("TOPODB_RECORDING=0 writes no spool marker", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-ss-"));
  try {
    assert.equal(run({ ...fixture("session-start.startup.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_RECORDING: "0" }), "");
    assert.ok(!existsSync(path.join(dir, "memory.warehouse")));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("TOPODB_HOOK_DEBUG dumps the raw payload (fixture-promotion path for the live field test)", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-ss-"));
  try {
    run({ ...fixture("session-start.startup.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_HOOK_DEBUG: "1" });
    const dumps = readdirSync(path.join(dir, "episodes"));
    assert.ok(dumps.some((f) => /^debug-sessionstart\.json$/i.test(f)), `no SessionStart debug dump in ${dumps}`);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("buildOutput emits hookSpecificOutput.additionalContext and clamps to additionalContextLimit", () => {
  assert.equal(buildOutput(null), null);
  assert.equal(buildOutput(""), null);
  // Normal recall passes through unclamped: the limit sits above recall's own
  // 6,000-char CHAR_CAP but under the ~2,500-token (~10,000-char) spill threshold.
  assert.ok(Number.isInteger(ADDITIONAL_CONTEXT_LIMIT));
  assert.ok(ADDITIONAL_CONTEXT_LIMIT >= 6000, "must not truncate what recall's CHAR_CAP already allows");
  assert.ok(ADDITIONAL_CONTEXT_LIMIT <= 10000, "must stay under Codex's ~2,500-token spill threshold");
  assert.deepEqual(buildOutput("hello"), { hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: "hello" } });
  // Oversized: tile the doc-derived base set until the rendered block exceeds the limit.
  const base = JSON.parse(readFileSync(path.join(HERE, "fixtures", "recall-oversized.json"), "utf8")).memories;
  const lines = [];
  while (lines.join("\n").length <= ADDITIONAL_CONTEXT_LIMIT * 2) for (const m of base) lines.push(`- ${m.props.content}`);
  const big = lines.join("\n");
  const out = buildOutput(big).hookSpecificOutput;
  assert.equal(out.hookEventName, "SessionStart");
  assert.ok(out.additionalContext.length <= ADDITIONAL_CONTEXT_LIMIT, `spilled: ${out.additionalContext.length} chars`);
  assert.equal(out.additionalContext.slice(0, 1000), big.slice(0, 1000), "clamp truncates the tail, keeps the head");
});
