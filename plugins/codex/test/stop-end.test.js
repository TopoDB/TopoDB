import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { readState, appendRetrieval } from "../core/recorder.js";
import { NUDGE_TEXT, SUBSTANTIVE_MIN_TOOLS } from "../core/hooks/stop.js";
import { appendSpool, artifactEvent } from "../core/warehouse-spool.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STOP = path.join(HERE, "..", "hooks", "stop.js");
const END = path.join(HERE, "..", "hooks", "session-end.js");
const fixture = (n) => JSON.parse(readFileSync(path.join(HERE, "fixtures", n), "utf8"));
const run = (hook, payload, env) => execFileSync(process.execPath, [hook], { input: typeof payload === "string" ? payload : JSON.stringify(payload), env: { ...process.env, ...env }, timeout: 15000 }).toString();
const spooled = (dir) => { const s = path.join(dir, "memory.warehouse", "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)) : []; };
const seedRetrieval = (dir, sessionId) =>
  appendRetrieval(dir, sessionId, { tool: "search_memories", query: "q", at: 1, channel: "text", returned: [{ id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", rank: 1, score: 0.5 }] }, new Map([["01ARZ3NDEKTSV4RRFFQ69G5FAV", "hello from memory"]]));
// Substantive-session signal: codex counts the session's own spooled tool
// artifacts (there is no parseable Codex transcript yet — rollout format is
// promoted from live payloads later, like the fixtures).
const seedToolUses = (dir, sessionId, n) => {
  for (let i = 0; i < n; i++) appendSpool(dir, sessionId, artifactEvent({ toolName: "Bash", toolInput: { command: `c${i}` }, toolResponse: `out${i}`, sessionId, scope: "0000000000000000000000000M", cwd: dir, harness: "codex" }), {});
};

test("stop is the load-bearing episode flush: state survives for a later sweep when no daemon is reachable", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-stop-"));
  try {
    seedRetrieval(dir, "codex-sess-0001");
    const out = run(STOP, { ...fixture("stop.turn-end.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir });
    assert.equal(out, "", "sub-substantive session: flush only, no nudge");
    assert.ok(readState(dir, "codex-sess-0001"), "no daemon → episode state kept, never dropped");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("stop nudges once for a substantive session, records the nudged marker, never nudges twice", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-stop-"));
  try {
    seedToolUses(dir, "codex-sess-0001", SUBSTANTIVE_MIN_TOOLS);
    const payload = { ...fixture("stop.turn-end.json"), cwd: dir };
    const out = run(STOP, payload, { TOPODB_PLUGIN_DATA: dir });
    assert.ok(out.includes(NUDGE_TEXT), `expected the capture nudge, got: ${out}`);
    assert.ok(JSON.parse(out), "nudge output must be the documented JSON hook output");
    assert.equal(readState(dir, "codex-sess-0001").nudged, true);
    assert.equal(run(STOP, payload, { TOPODB_PLUGIN_DATA: dir }), "", "once per session");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("stop loop guard: the auto-submitted follow-up turn (stop_hook_active) never re-nudges", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-stop-"));
  try {
    seedToolUses(dir, "codex-sess-0001", SUBSTANTIVE_MIN_TOOLS);
    assert.equal(run(STOP, { ...fixture("stop.loop-guard.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.notEqual(readState(dir, "codex-sess-0001")?.nudged, true, "the guarded turn must not burn the once-per-session marker");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("stop stays silent on thin sessions, kill switch, and garbage stdin", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-stop-"));
  try {
    seedToolUses(dir, "codex-sess-0001", SUBSTANTIVE_MIN_TOOLS - 1);
    assert.equal(run(STOP, { ...fixture("stop.turn-end.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    const dir2 = mkdtempSync(path.join(tmpdir(), "cdx-stop-"));
    try {
      seedToolUses(dir2, "codex-sess-0001", SUBSTANTIVE_MIN_TOOLS);
      assert.equal(run(STOP, { ...fixture("stop.turn-end.json"), cwd: dir2 }, { TOPODB_PLUGIN_DATA: dir2, TOPODB_RECORDING: "0" }), "");
      assert.equal(run(STOP, readFileSync(path.join(HERE, "fixtures", "garbage.txt"), "utf8"), { TOPODB_PLUGIN_DATA: dir2 }), "");
    } finally { rmSync(dir2, { recursive: true, force: true }); }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("session-end: marker + DETACHED flusher — the hook exits well under Codex's 3 s hard cap and never waits", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-end-"));
  try {
    seedRetrieval(dir, "codex-sess-0001");
    const t0 = Date.now();
    // A 5 s daemon-connect budget: an inline flush, or a flusher holding the
    // hook's stdio (not disowned/unref'd), would blow the 3 s assertion.
    const out = run(END, { ...fixture("session-end.exit.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir, TOPODB_DAEMON_CONNECT_MS: "5000" });
    const elapsed = Date.now() - t0;
    assert.equal(out, "");
    assert.ok(elapsed < 3000, `SessionEnd hook took ${elapsed}ms — Codex kills it at 3000ms`);
    assert.ok(spooled(dir).some((e) => e.marker?.type === "session_end" && e.marker.harness === "codex"));
    assert.ok(readState(dir, "codex-sess-0001"), "advisory only: with no daemon the state is kept, Stop already flushed");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("session-end 30-min-idle reason writes the same advisory marker; background agents and garbage produce nothing", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cdx-end-"));
  try {
    assert.equal(run(END, { ...fixture("session-end.idle.json"), cwd: dir }, { TOPODB_PLUGIN_DATA: dir }), "");
    assert.ok(spooled(dir).some((e) => e.marker?.type === "session_end"));
    const dir2 = mkdtempSync(path.join(tmpdir(), "cdx-end-"));
    try {
      assert.equal(run(END, { ...fixture("session-end.exit.json"), cwd: dir2, is_background_agent: true }, { TOPODB_PLUGIN_DATA: dir2 }), "");
      assert.equal(run(END, readFileSync(path.join(HERE, "fixtures", "garbage.txt"), "utf8"), { TOPODB_PLUGIN_DATA: dir2 }), "");
      assert.ok(!existsSync(path.join(dir2, "memory.warehouse")));
    } finally { rmSync(dir2, { recursive: true, force: true }); }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
