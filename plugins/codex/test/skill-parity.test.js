// plugins/codex/test/skill-parity.test.js
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const tail = (p) => { const t = readFileSync(p, "utf8"); const i = t.indexOf("## Recall before you guess"); assert.ok(i > 0, `${p}: missing '## Recall before you guess'`); return t.slice(i).replace(/\s+/g, " "); };
test("the tool-facing part of SKILL.md matches the Claude Code plugin's (deliberate drift only)", () => {
  assert.equal(tail(path.join(ROOT, "skills", "topodb-memory", "SKILL.md")), tail(path.join(ROOT, "..", "claude-code", "skills", "topodb-memory", "SKILL.md")));
});
