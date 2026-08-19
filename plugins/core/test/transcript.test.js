import { test } from "node:test";
import assert from "node:assert/strict";
import { parseClaude, parseCursor, assistantTextOrNull } from "../transcript.js";

const claude = [
  JSON.stringify({ type: "user", message: { content: "hi" } }),
  JSON.stringify({ type: "assistant", message: { content: [{ type: "text", text: "hello" }, { type: "tool_use", name: "Read" }] } }),
  "garbage line",
  JSON.stringify({ type: "assistant", message: { content: [{ type: "tool_use", name: "Bash" }, { type: "text", text: "done" }] } }),
].join("\n");

test("parseClaude: assistant text joined, tool_use counted, garbage skipped", () => {
  const p = parseClaude(claude);
  assert.equal(p.toolUses, 2);
  assert.equal(p.assistantText, "hello\ndone");
  assert.equal(p.recognized, true);
  assert.deepEqual(parseClaude(""), { assistantText: "", toolUses: 0, recognized: false });
});

// SYNTHETIC until spec D4 pins Cursor's real transcript JSONL.
const cursor = [
  JSON.stringify({ role: "user", content: "hi" }),
  JSON.stringify({ role: "assistant", content: [{ type: "text", text: "hello" }, { type: "tool_call", name: "Shell", arguments: { command: "ls" } }] }),
  JSON.stringify({ role: "assistant", content: "done", tool_calls: [{ name: "Read" }] }),
].join("\n");

test("parseCursor: role/type tolerant, tool calls counted, recognized", () => {
  const p = parseCursor(cursor);
  assert.equal(p.assistantText, "hello\ndone");
  assert.equal(p.toolUses, 2);
  assert.equal(p.recognized, true);
});

test("unrecognized transcripts yield null assistant text (no usage judgment)", () => {
  const p = parseCursor('{"weird":1}\n{"also":"weird"}');
  assert.equal(p.recognized, false);
  assert.equal(assistantTextOrNull(p), null);
  assert.equal(assistantTextOrNull(parseCursor(cursor)), "hello\ndone");
});
