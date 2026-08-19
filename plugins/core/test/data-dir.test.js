import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { resolveDataDir, CLAUDE_SHARED_REL, DEFAULT_REL } from "../data-dir.js";

const home = path.join("/home", "u");
test("TOPODB_PLUGIN_DATA wins over everything", () => {
  const r = resolveDataDir({ TOPODB_PLUGIN_DATA: "/x", CLAUDE_PLUGIN_DATA: "/y" }, { homedir: home, exists: () => true });
  assert.deepEqual(r, { dir: "/x", reason: "override" });
});
test("CLAUDE_PLUGIN_DATA is second", () => {
  const r = resolveDataDir({ CLAUDE_PLUGIN_DATA: "/y" }, { homedir: home, exists: () => true });
  assert.deepEqual(r, { dir: "/y", reason: "claude-env" });
});
test("shares the Claude Code data dir when it exists", () => {
  const claude = path.join(home, ...CLAUDE_SHARED_REL);
  const r = resolveDataDir({}, { homedir: home, exists: (p) => p === claude });
  assert.deepEqual(r, { dir: claude, reason: "claude-shared" });
});
test("falls back to ~/.topodb/plugin-data", () => {
  const r = resolveDataDir({}, { homedir: home, exists: () => false });
  assert.deepEqual(r, { dir: path.join(home, ...DEFAULT_REL), reason: "default" });
});
test("blank override strings are ignored", () => {
  const r = resolveDataDir({ TOPODB_PLUGIN_DATA: "  ", CLAUDE_PLUGIN_DATA: "" }, { homedir: home, exists: () => false });
  assert.equal(r.reason, "default");
});
