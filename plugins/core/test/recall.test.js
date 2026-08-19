import { test } from "node:test";
import assert from "node:assert/strict";
import { recallForSessionStart, renderHealth, renderInjection } from "../hooks/recall.js";

function fakeClient(responses) {
  const calls = [];
  return { calls, async call(name, args) { calls.push([name, args]); const r = responses[name]; if (r instanceof Error) throw r; return typeof r === "function" ? r(args) : (r ?? {}); }, close() {} };
}
test("renders up to keep memories ranked by access count with entity names and a health nudge", async () => {
  const mems = Array.from({ length: 10 }, (_, i) => ({ id: `M${i}`, props: { content: `memory ${i}` } }));
  const client = fakeClient({
    recent_memories: { memories: mems },
    access_stats: ({ id }) => ({ found: true, access_count: id === "M7" ? 99 : 1 }),
    get_edges: ({ from_id }) => ({ edges: from_id === "M7" ? [{ to: "E1" }] : [] }),
    get_node: { node: { props: { name: "Drew" } } },
    memory_health: { needs_attention: true, duplicate_pairs: 2, supersession_pairs: 0, orphan_count: 1, stale_count: 0 },
  });
  const out = await recallForSessionStart(client);
  assert.ok(out.startsWith("## TopoDB memory (this project)"));
  const lines = out.split("\n");
  assert.match(lines[1], /memory 7 \[entities: Drew\]/);
  assert.equal(lines.filter((l) => l.startsWith("- ")).length, 8);
  assert.match(out, /🧹 Memory hygiene: 2 duplicate pairs, 1 orphan/);
  assert.match(out, /Deeper recall: search_memories/);
});
test("empty store → null; failures in decoration never break the injection", async () => {
  assert.equal(await recallForSessionStart(fakeClient({ recent_memories: { memories: [] } })), null);
  const client = fakeClient({ recent_memories: { memories: [{ id: "A", props: { content: "x" } }] }, access_stats: new Error("boom"), get_edges: new Error("boom"), memory_health: new Error("boom") });
  assert.match(await recallForSessionStart(client), /- x/);
});
test("renderHealth/renderInjection are exported", () => {
  assert.equal(renderHealth(null), null);
  assert.equal(renderInjection([]), null);
});
