import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => JSON.parse(readFileSync(path.join(ROOT, p), "utf8"));

test("plugin.json is a valid Cursor manifest pointing at files that exist", () => {
  const m = read(".cursor-plugin/plugin.json");
  assert.equal(m.name, "topodb");
  assert.match(m.version, /^\d+\.\d+\.\d+$/);
  for (const k of ["mcpServers", "hooks", "rules", "skills", "commands", "logo"]) assert.ok(existsSync(path.join(ROOT, m[k])), `${k} → ${m[k]} missing`);
});
test("mcp.json launches node on the plugin's launch.js via ${CURSOR_PLUGIN_ROOT}", () => {
  const s = read("mcp.json").mcpServers.topodb;
  assert.equal(s.command, "node");
  assert.deepEqual(s.args, ["${CURSOR_PLUGIN_ROOT}/launch.js"]);
});
test("root marketplace lists this plugin at plugins/cursor", () => {
  const mk = JSON.parse(readFileSync(path.join(ROOT, "..", "..", ".cursor-plugin", "marketplace.json"), "utf8"));
  const e = mk.plugins.find((p) => p.name === "topodb");
  assert.equal(e.source, "plugins/cursor");
});
