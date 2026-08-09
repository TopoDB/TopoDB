import { test } from "node:test";
import assert from "node:assert/strict";
import { extractTask, skipSet, renderSubagentContext } from "../hooks/subagent-start.js";

const jsonl = (...objs) => objs.map((o) => JSON.stringify(o)).join("\n");

test("extractTask: first user message, string content", () => {
  const t = jsonl(
    { type: "user", message: { role: "user", content: "Implement the widget parser" } },
    { type: "assistant", message: { role: "assistant", content: "ok" } },
  );
  assert.equal(extractTask(t), "Implement the widget parser");
});

test("extractTask: array content is flattened to text", () => {
  const t = jsonl({
    type: "user",
    message: { role: "user", content: [{ type: "text", text: "part one " }, { type: "text", text: "part two" }] },
  });
  assert.equal(extractTask(t), "part one part two");
});

test("extractTask: caps at 1000 chars", () => {
  const t = jsonl({ type: "user", message: { role: "user", content: "z".repeat(5000) } });
  assert.equal(extractTask(t).length, 1000);
});

test("extractTask: null when no user message / empty / unparseable", () => {
  assert.equal(extractTask(jsonl({ type: "assistant", message: { role: "assistant", content: "x" } })), null);
  assert.equal(extractTask(""), null);
  assert.equal(extractTask("{not json"), null);
  assert.equal(extractTask(jsonl({ type: "user", message: { role: "user", content: "   " } })), null);
});

test("skipSet: default skips Explore and Plan (case-insensitive)", () => {
  const s = skipSet({});
  assert.ok(s.has("explore") && s.has("plan"));
  assert.ok(!s.has("general-purpose"));
});

test("skipSet: env replaces default; empty string skips nothing", () => {
  assert.ok(skipSet({ TOPODB_SUBAGENT_SKIP: "general-purpose, Foo" }).has("foo"));
  assert.ok(!skipSet({ TOPODB_SUBAGENT_SKIP: "general-purpose" }).has("explore"));
  assert.equal(skipSet({ TOPODB_SUBAGENT_SKIP: "" }).size, 0);
});

test("renderSubagentContext: header, hit lines, affordance trailer; null when no hits", () => {
  assert.equal(renderSubagentContext([]), null);
  assert.equal(renderSubagentContext([{ id: "1", props: {} }]), null); // no content -> null
  const out = renderSubagentContext([
    { id: "1", props: { content: "The parser lives in widget.rs" } },
    { id: "2", props: { content: "Prefer streaming over buffering" } },
  ]);
  assert.match(out, /^## Relevant project memory/m);
  assert.match(out, /The parser lives in widget\.rs/);
  assert.match(out, /Recall more: search_memories\. Store: remember\.$/);
});
