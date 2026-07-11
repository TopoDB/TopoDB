import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EpisodeBuffer,
  extractText,
  tokenize,
  isUsed,
  buildEpisodeBatch,
  toRetrievalRecord,
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

// Fixtures below are the ACTUAL JSON returned by the branch-built
// topodb-mcp binary (target/debug/topodb-mcp.exe), captured by driving it
// over stdio with a throwaway script — not guessed. See task-5-report.md
// for the full capture transcript.

test("toRetrievalRecord: search_memories hit -> text-channel RetrievalRecord + contents", () => {
  const result = {
    hits: [
      {
        node: {
          id: "01KX99JAFBKV80B89YCJ4QHM47",
          scope: "shared",
          label: "Memory",
          props: { content: "rust ownership borrow checker rules" },
        },
        score: 0.5753642320632935,
      },
    ],
  };
  const cap = toRetrievalRecord("search_memories", { query: "borrow checker", k: 5 }, result);
  assert.ok(cap);
  assert.equal(cap.record.channel, "text");
  assert.equal(cap.record.query, "borrow checker");
  assert.deepEqual(cap.record.returned, [
    { id: "01KX99JAFBKV80B89YCJ4QHM47", rank: 0, score: 0.5753642320632935 },
  ]);
  assert.equal(cap.contents.get("01KX99JAFBKV80B89YCJ4QHM47"), "rust ownership borrow checker rules");
});

test("toRetrievalRecord: search_memories with no hits -> empty returned, no contents", () => {
  const cap = toRetrievalRecord("search_memories", { query: "nothing", k: 5 }, { hits: [] });
  assert.ok(cap);
  assert.equal(cap.record.returned.length, 0);
  assert.equal(cap.contents.size, 0);
});

test("toRetrievalRecord: traverse subgraph -> graph-channel RetrievalRecord + contents (edges use `type`, not `ty`)", () => {
  const result = {
    subgraph: {
      edges: [
        {
          from: "01KX99JAFFNZK9QQXRWZZ317DD",
          id: "01KX99JAFH5EH7H3WJFRWRTWRE",
          props: {},
          scope: "shared",
          to: "01KX99JAFBKV80B89YCJ4QHM47",
          type: "RELATES_TO",
          valid_from: 1783797197297,
          valid_to: null,
        },
      ],
      nodes: [
        {
          id: "01KX99JAFFNZK9QQXRWZZ317DD",
          label: "Entity",
          props: { name: "RustLang" },
          scope: "shared",
        },
        {
          id: "01KX99JAFBKV80B89YCJ4QHM47",
          label: "Memory",
          props: { content: "rust ownership borrow checker rules" },
          scope: "shared",
        },
      ],
    },
  };
  const cap = toRetrievalRecord("traverse", { seed_id: "01KX99JAFFNZK9QQXRWZZ317DD", max_hops: 2 }, result);
  assert.ok(cap);
  assert.equal(cap.record.channel, "graph");
  assert.equal(cap.record.query, "01KX99JAFFNZK9QQXRWZZ317DD");
  assert.deepEqual(cap.record.returned, [
    { id: "01KX99JAFFNZK9QQXRWZZ317DD", rank: 0, score: 0 },
    { id: "01KX99JAFBKV80B89YCJ4QHM47", rank: 1, score: 0 },
  ]);
  // Entity node has no `content` prop -> not collected; Memory node's does.
  assert.equal(cap.contents.has("01KX99JAFFNZK9QQXRWZZ317DD"), false);
  assert.equal(cap.contents.get("01KX99JAFBKV80B89YCJ4QHM47"), "rust ownership borrow checker rules");
});

test("toRetrievalRecord: unknown tool or malformed result -> undefined", () => {
  assert.equal(toRetrievalRecord("create_memory", {}, { id: "x" }), undefined);
  assert.equal(toRetrievalRecord("search_memories", {}, { hits: "not-an-array" }), undefined);
  assert.equal(toRetrievalRecord("traverse", {}, { subgraph: {} }), undefined);
  assert.equal(toRetrievalRecord("search_memories", {}, undefined), undefined);
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
