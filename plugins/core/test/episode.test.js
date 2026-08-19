import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { flushEpisode } from "../hooks/episode.js";
import { appendRetrieval, readState, buildEpisodeBatch } from "../recorder.js";

function seed(dir, sid) {
  appendRetrieval(dir, sid, { tool: "search_memories", query: "q", at: 1, channel: "text", returned: [{ id: "01ARZ3NDEKTSV4RRFFQ69G5FAV", rank: 1, score: 0.5 }] }, new Map([["01ARZ3NDEKTSV4RRFFQ69G5FAV", "the daemon owns the database file"]]));
}
function fakeConnect(store) {
  return async () => ({ async call(name, args) { store.push([name, args]); return { ok: true }; }, close() {} });
}
test("judges usage from assistant text, submits a batch, deletes state", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "ep-")); const sent = [];
  try {
    seed(dir, "s1");
    const r = await flushEpisode({ dataDir: dir, env: {}, projectDir: dir, sessionId: "s1", assistantText: "yes the daemon owns the database file", reason: "completed", connect: fakeConnect(sent), now: () => 2 });
    assert.equal(r, "flushed");
    const cmds = sent[0][1].commands;
    assert.equal(cmds[0].label, "Episode"); assert.equal(cmds[0].props.usage_judged, true); assert.equal(cmds[0].props.reason, "completed");
    assert.ok(cmds.some((c) => c.type === "USED"));
    assert.equal(readState(dir, "s1"), null);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("null assistant text → no USED links and usage_judged=false", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "ep-")); const sent = [];
  try {
    seed(dir, "s2");
    assert.equal(await flushEpisode({ dataDir: dir, env: {}, projectDir: dir, sessionId: "s2", assistantText: null, reason: "", connect: fakeConnect(sent) }), "flushed");
    const cmds = sent[0][1].commands;
    assert.equal(cmds[0].props.usage_judged, false);
    assert.ok(!cmds.some((c) => c.type === "USED"));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("no state → no-state; no daemon → no-daemon (state kept); disabled → disabled", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "ep-"));
  try {
    assert.equal(await flushEpisode({ dataDir: dir, env: {}, projectDir: dir, sessionId: "none", assistantText: "", reason: "", connect: async () => null }), "no-state");
    seed(dir, "s3");
    assert.equal(await flushEpisode({ dataDir: dir, env: {}, projectDir: dir, sessionId: "s3", assistantText: "", reason: "", connect: async () => null }), "no-daemon");
    assert.ok(readState(dir, "s3"));
    assert.equal(await flushEpisode({ dataDir: dir, env: { TOPODB_RECORDING: "0" }, projectDir: dir, sessionId: "s3", assistantText: "", reason: "", connect: async () => null }), "disabled");
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("buildEpisodeBatch defaults usage_judged to true", () => {
  const cmds = buildEpisodeBatch({ state: { startedAt: 1, retrievals: [], contents: {} }, outcome: "success", failure: "", endedAt: 2, used: new Map() });
  assert.equal(cmds[0].props.usage_judged, true);
});
