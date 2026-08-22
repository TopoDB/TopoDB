import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (p) => JSON.parse(readFileSync(path.join(ROOT, p), "utf8"));

test("plugin.json is a valid Codex manifest pointing at files that exist", () => {
  const m = read(".codex-plugin/plugin.json");
  assert.equal(m.name, "topodb");
  assert.match(m.version, /^\d+\.\d+\.\d+$/);
  assert.equal(m.mcpServers, "./.mcp.json");
  assert.equal(m.hooks, "./hooks/hooks.json");
  assert.equal(m.skills, "./skills/");
  for (const k of ["mcpServers", "hooks", "skills", "logo"]) assert.ok(existsSync(path.join(ROOT, m[k])), `${k} → ${m[k]} missing`);
});

test(".mcp.json launches node on the plugin's launch.js via ${PLUGIN_ROOT}", () => {
  const s = read(".mcp.json").mcpServers.topodb;
  assert.equal(s.command, "node");
  assert.deepEqual(s.args, ["${PLUGIN_ROOT}/launch.js"]);
});

test("hooks.json wires the five handlers; compact re-fires the same session-start script", () => {
  const h = read("hooks/hooks.json").hooks;
  const ssCmd = "node ${PLUGIN_ROOT}/hooks/session-start.js";
  assert.equal(h.SessionStart.length, 2);
  assert.equal(h.SessionStart[0].matcher, "startup|resume|clear");
  assert.equal(h.SessionStart[1].matcher, "compact");
  for (const e of h.SessionStart) assert.deepEqual(e.hooks, [{ type: "command", command: ssCmd }]);
  assert.equal(h.PostToolUse.length, 1);
  assert.ok(!("matcher" in h.PostToolUse[0]), "PostToolUse captures ALL tools — no matcher");
  assert.deepEqual(h.PostToolUse[0].hooks, [{ type: "command", command: "node ${PLUGIN_ROOT}/hooks/warehouse-capture.js", async: true }]);
  assert.deepEqual(h.Stop, [{ hooks: [{ type: "command", command: "node ${PLUGIN_ROOT}/hooks/stop.js" }] }]);
  assert.deepEqual(h.SessionEnd, [{ hooks: [{ type: "command", command: "node ${PLUGIN_ROOT}/hooks/session-end.js" }] }]);
});

test("hook command strings are static forever: type command, no flags/env/runtime interpolation, scripts ship", () => {
  // Codex trust-hashes each hook definition; any change re-prompts in /hooks.
  const h = read("hooks/hooks.json").hooks;
  const defs = Object.values(h).flat().flatMap((e) => e.hooks);
  assert.equal(defs.length, 5);
  for (const d of defs) {
    assert.equal(d.type, "command");
    assert.match(d.command, /^node \$\{PLUGIN_ROOT\}\/hooks\/[a-z][a-z-]*\.js$/, `not the frozen form: ${d.command}`);
    assert.equal(d.command.split("${").length, 2, `only the literal \${PLUGIN_ROOT} is allowed: ${d.command}`);
    const script = d.command.match(/hooks\/[a-z-]+\.js$/)[0];
    assert.ok(existsSync(path.join(ROOT, script)), `${script} missing`);
  }
});

test("root Codex marketplace (.agents/plugins/marketplace.json) lists this plugin at plugins/codex", () => {
  const mk = JSON.parse(readFileSync(path.join(ROOT, "..", "..", ".agents", "plugins", "marketplace.json"), "utf8"));
  assert.equal(mk.name, "topodb");
  const e = mk.plugins.find((p) => p.name === "topodb");
  assert.equal(e.source, "plugins/codex");
  assert.ok(typeof e.description === "string" && e.description.length > 0);
});
