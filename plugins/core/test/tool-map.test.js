import { test } from "node:test";
import assert from "node:assert/strict";
import { fromCursorToolUse, CURSOR_TOOL_NAMES } from "../tool-map.js";
import { artifactEvent } from "../warehouse-spool.js";

test("maps Cursor tool names onto the artifact vocabulary and normalizes input keys", () => {
  assert.equal(CURSOR_TOOL_NAMES.Shell, "Bash");
  const sh = fromCursorToolUse({ tool_name: "Shell", tool_input: { command: "ls" }, tool_output: JSON.stringify({ output: "a\nb" }) });
  assert.deepEqual(sh, { toolName: "Bash", toolInput: { command: "ls" }, toolResponse: { output: "a\nb" } });
  const rd = fromCursorToolUse({ tool_name: "Read", tool_input: { path: "/p/a.rs" }, tool_output: "fn a(){}" });
  assert.deepEqual(rd, { toolName: "Read", toolInput: { file_path: "/p/a.rs" }, toolResponse: "fn a(){}" });
  const ed = fromCursorToolUse({ tool_name: "StrReplace", tool_input: { filePath: "/p/a.rs", oldString: "a", newString: "b" }, tool_output: "{}" });
  assert.deepEqual(ed, { toolName: "Edit", toolInput: { file_path: "/p/a.rs", old_string: "a", new_string: "b" }, toolResponse: {} });
  const wr = fromCursorToolUse({ tool_name: "Write", tool_input: { target_file: "/p/n.rs", contents: "new" }, tool_output: "" });
  assert.deepEqual(wr, { toolName: "Write", toolInput: { file_path: "/p/n.rs", content: "new" }, toolResponse: "" });
  const gr = fromCursorToolUse({ tool_name: "Grep", tool_input: { query: "foo" }, tool_output: "a.rs:1:foo" });
  assert.deepEqual(gr, { toolName: "Grep", toolInput: { pattern: "foo" }, toolResponse: "a.rs:1:foo" });
});

test("drops MCP and unknown tools; tolerates missing input", () => {
  assert.equal(fromCursorToolUse({ tool_name: "MCP:topodb", tool_input: {}, tool_output: "{}" }), null);
  assert.equal(fromCursorToolUse({ tool_name: "mcp__topodb__remember", tool_input: {}, tool_output: "{}" }), null);
  assert.equal(fromCursorToolUse({ tool_name: "Task", tool_input: {}, tool_output: "{}" }), null);
  assert.equal(fromCursorToolUse({ tool_name: "Shell", tool_input: undefined, tool_output: undefined }).toolInput.command, undefined);
});

test("the mapped triple flows into artifactEvent", () => {
  const t = fromCursorToolUse({ tool_name: "Shell", tool_input: { command: "pwd" }, tool_output: "/p" });
  const ev = artifactEvent({ ...t, sessionId: "s", scope: "01ARZ3NDEKTSV4RRFFQ69G5FAV", cwd: "/p", harness: "cursor", nowMs: 1 });
  assert.equal(ev.artifact.type, "command"); assert.equal(ev.artifact.locator, "pwd");
  assert.equal(ev.artifact.content, "/p"); assert.equal(ev.source.harness, "cursor");
});
