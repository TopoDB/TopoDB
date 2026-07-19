import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, readdirSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import {
  extractText, tokenize, isUsed, toRetrievalRecord, buildEpisodeBatch,
  appendRetrieval, readState, deleteState, sweepStale, stateFilePath,
} from "../recorder.js";

test("text helpers match pi semantics", () => {
  assert.equal(extractText([{ type: "text", text: "a" }, { type: "image" }, { type: "text", text: "b" }]), "ab");
  assert.deepEqual([...tokenize("Foo foo ba bar-baz 42x")], ["foo", "bar", "baz", "42x"]);
  assert.equal(isUsed("alpha beta gamma delta", "we used alpha and beta and gamma here"), true);
  assert.equal(isUsed("alpha beta gamma delta", "only alpha appears"), false);
  assert.equal(isUsed("", "anything"), false);
});

test("toRetrievalRecord handles all three tools", () => {
  const search = toRetrievalRecord("search_memories", { query: "q" }, {
    hits: [{ node: { id: "01A", props: { content: "hello world content" } }, score: 0.03 }],
  });
  assert.equal(search.record.channel, "text");
  assert.equal(search.record.query, "q");
  assert.deepEqual(search.record.returned, [{ id: "01A", rank: 0, score: 0.03 }]);
  assert.equal(search.contents.get("01A"), "hello world content");

  const trav = toRetrievalRecord("traverse", { seed_id: "01S" }, {
    subgraph: { nodes: [{ id: "01B", props: {} }], edges: [] },
  });
  assert.equal(trav.record.channel, "graph");

  const recent = toRetrievalRecord("recent_memories", {}, {
    memories: [{ id: "01C", props: { content: "c" } }],
  });
  assert.equal(recent.record.channel, "recent");
  assert.deepEqual(recent.record.returned, [{ id: "01C", rank: 0, score: 0 }]);

  assert.equal(toRetrievalRecord("get_node", {}, {}), undefined);
  assert.equal(toRetrievalRecord("search_memories", {}, { nope: 1 }), undefined);
});

test("state file lifecycle: append, read, delete, malformed, sweep", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "topodb-rec-"));
  try {
    const sid = "sess-1";
    const rec = { query: "q", at: 1, channel: "text", returned: [{ id: "01A", rank: 0, score: 1 }] };
    appendRetrieval(dir, sid, rec, new Map([["01A", "content a"]]));
    appendRetrieval(dir, sid, { ...rec, query: "q2" }, new Map());
    const state = readState(dir, sid);
    assert.equal(state.retrievals.length, 2);
    assert.equal(state.contents["01A"], "content a");
    assert.ok(state.startedAt > 0);

    deleteState(dir, sid);
    assert.equal(readState(dir, sid), null);

    // Malformed file: read returns null and removes it.
    writeFileSync(stateFilePath(dir, "bad"), "{not json");
    assert.equal(readState(dir, "bad"), null);
    assert.equal(readState(dir, "bad"), null); // gone now, still null

    // Sweep: an old file goes, a fresh one stays.
    appendRetrieval(dir, "old", rec, new Map());
    appendRetrieval(dir, "fresh", rec, new Map());
    const oldPath = stateFilePath(dir, "old");
    const past = new Date(Date.now() - 8 * 24 * 3600 * 1000);
    utimesSync(oldPath, past, past);
    sweepStale(dir);
    const left = readdirSync(path.dirname(oldPath));
    assert.deepEqual(left.sort(), ["fresh.json"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("buildEpisodeBatch emits pi's exact vocabulary", () => {
  const state = {
    startedAt: 100,
    retrievals: [
      { query: "q", at: 5, channel: "text", returned: [{ id: "01A", rank: 0, score: 1 }] },
    ],
    contents: { "01A": "alpha beta" },
  };
  const cmds = buildEpisodeBatch({
    state,
    outcome: "success",
    failure: "",
    endedAt: 200,
    used: new Map([[0, new Set(["01A"])]]),
  });
  assert.deepEqual(cmds[0], {
    op: "create_node",
    label: "Episode",
    props: {
      goal: "", strategy: "", outcome: "success", started_at: 100, ended_at: 200,
      turns: 1, tokens: 0, confidence: 0.5, failure: "",
    },
  });
  assert.deepEqual(cmds[1], { op: "create_node", label: "RetrievalEvent", props: { query: "q", at: 5 } });
  assert.deepEqual(cmds[2], { op: "link", from: "#0", to: "#1", type: "ISSUED" });
  assert.deepEqual(cmds[3], {
    op: "link", from: "#1", to: "01A", type: "RETURNED",
    props: { rank: 0, score: 1, channel: "text" },
  });
  assert.deepEqual(cmds[4], { op: "link", from: "#1", to: "01A", type: "USED" });
  assert.equal(cmds.length, 5);
});
