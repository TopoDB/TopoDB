// plugins/cursor/test/skill-parity.test.js
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const tail = (p) => { const t = readFileSync(p, "utf8"); const i = t.indexOf("## Recall before you guess"); assert.ok(i > 0, `${p}: missing '## Recall before you guess'`); return t.slice(i).replace(/\s+/g, " "); };
test("the tool-facing part of SKILL.md matches the Claude Code plugin's (deliberate drift only)", () => {
  assert.equal(tail(path.join(ROOT, "skills", "topodb-memory", "SKILL.md")), tail(path.join(ROOT, "..", "claude-code", "skills", "topodb-memory", "SKILL.md")));
});
test("rules and commands exist with the expected frontmatter", () => {
  const rule = readFileSync(path.join(ROOT, "rules", "topodb-memory.mdc"), "utf8");
  assert.match(rule, /^---\n[\s\S]*alwaysApply: true[\s\S]*\n---\n/);
  assert.match(rule, /search_memories/); assert.match(rule, /remember/);
  for (const c of ["recall.md", "remember.md"]) assert.ok(existsSync(path.join(ROOT, "commands", c)));
  for (const d of ["rules", "skills", "commands"]) assert.ok(!existsSync(path.join(ROOT, d, ".gitkeep")));
});
