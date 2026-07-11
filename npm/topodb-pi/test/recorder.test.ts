import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EpisodeBuffer,
  extractText,
  tokenize,
  isUsed,
  buildEpisodeBatch,
} from "../src/recorder.ts";

test("extractText: string content passes through", () => {
  assert.equal(extractText("fix the bug"), "fix the bug");
});

test("extractText: block arrays concat only text blocks", () => {
  assert.equal(
    extractText([
      { type: "text", text: "hello " },
      { type: "thinking", thinking: "IGNORED" },
      { type: "text", text: "world" },
    ]),
    "hello world",
  );
});

test("tokenize: lowercase alnum runs, length>=3, deduped", () => {
  assert.deepEqual(
    [...tokenize("The Borrow-Checker, the borrow checker!! a b")],
    ["the", "borrow", "checker"],
  );
});

test("isUsed: >=50% of memory tokens appearing in text counts as used", () => {
  const mem = "rust ownership rules"; // tokens: rust, ownership, rules
  assert.equal(isUsed(mem, "we applied rust ownership principles"), true); // 2/3
  assert.equal(isUsed(mem, "we discussed rust briefly"), false); // 1/3
  assert.equal(isUsed("", "anything"), false); // no tokens -> never used
});

test("buildEpisodeBatch: exact command array shape", () => {
  const buf = new EpisodeBuffer();
  buf.start(1000);
  buf.bumpTurns();
  buf.bumpTurns();
  buf.addRetrieval({
    query: "bug history",
    at: 1500,
    channel: "text",
    returned: [
      { id: "01MEMAAAAAAAAAAAAAAAAAAAAA", rank: 0, score: 0.9 },
      { id: "01MEMBBBBBBBBBBBBBBBBBBBBB", rank: 1, score: 0.4 },
    ],
  });
  const cmds = buildEpisodeBatch({
    buffer: buf,
    goal: "fix the bug",
    outcome: "success",
    failure: "",
    endedAt: 9000,
    tokens: 1234,
    used: new Map([[0, new Set(["01MEMAAAAAAAAAAAAAAAAAAAAA"])]]),
    policyVersionId: "01POLICYVVVVVVVVVVVVVVVVVV",
  });
  assert.deepEqual(cmds, [
    { op: "create_node", label: "Episode", props: {
        goal: "fix the bug", strategy: "", outcome: "success",
        started_at: 1000, ended_at: 9000, turns: 2, tokens: 1234,
        confidence: 0.5, failure: "" } },
    { op: "create_node", label: "RetrievalEvent",
      props: { query: "bug history", at: 1500 } },
    { op: "link", from: "#0", to: "#1", type: "ISSUED" },
    { op: "link", from: "#1", to: "01MEMAAAAAAAAAAAAAAAAAAAAA", type: "RETURNED",
      props: { rank: 0, score: 0.9, channel: "text" } },
    { op: "link", from: "#1", to: "01MEMBBBBBBBBBBBBBBBBBBBBB", type: "RETURNED",
      props: { rank: 1, score: 0.4, channel: "text" } },
    { op: "link", from: "#1", to: "01MEMAAAAAAAAAAAAAAAAAAAAA", type: "USED" },
    { op: "link", from: "#0", to: "01POLICYVVVVVVVVVVVVVVVVVV", type: "USED_POLICY" },
  ]);
});

test("buildEpisodeBatch: no policy -> no USED_POLICY command", () => {
  const buf = new EpisodeBuffer();
  buf.start(1);
  const cmds = buildEpisodeBatch({
    buffer: buf, goal: "g", outcome: "failure", failure: "aborted",
    endedAt: 2, tokens: 0, used: new Map(),
  }) as Array<{ type?: string }>;
  assert.equal(cmds.length, 1);
  assert.ok(!cmds.some((c) => c.type === "USED_POLICY"));
});
