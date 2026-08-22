import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LAUNCH = path.join(HERE, "..", "launch.js");

function fakeMcp(dir) {
  const fake = path.join(dir, "fake-mcp.mjs");
  writeFileSync(
    fake,
    `#!/usr/bin/env node
if (process.argv.includes("--socket")) { setInterval(() => {}, 1 << 30); }
else {
  process.stdin.on("data", (buf) => {
    for (const line of buf.toString().split("\\n")) {
      if (!line.trim()) continue;
      let msg; try { msg = JSON.parse(line); } catch { continue; }
      if (msg.method === "initialize") {
        process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { protocolVersion: "2024-11-05", capabilities: { tools: { listChanged: false } }, serverInfo: { name: "fake-mcp", version: "1" }, instructions: "real" } }) + "\\n");
      }
    }
  });
}
`,
  );
  chmodSync(fake, 0o755);
  return fake;
}

test("Cursor-imported Claude plugin still serves tools when CLAUDE_PLUGIN_DATA is unset", async () => {
  const tmp = mkdtempSync(path.join(tmpdir(), "cc-launch-cursor-import-"));
  const dataDir = path.join(tmp, "data");
  mkdirSync(dataDir);
  const fake = fakeMcp(tmp);
  const env = { ...process.env };
  delete env.CLAUDE_PLUGIN_DATA;
  const child = spawn(process.execPath, [LAUNCH], {
    env: {
      ...env,
      TOPODB_PLUGIN_DATA: dataDir,
      CURSOR_PROJECT_DIR: HERE,
      TOPODB_MCP_SERVER_BIN: fake,
      TOPODB_DAEMON_CONNECT_MS: "400",
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let out = "";
  child.stdout.on("data", (d) => (out += d));
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "t", version: "0" } } }) + "\n");
  const deadline = Date.now() + 8000;
  while (!out.includes('"id":1') && Date.now() < deadline) await new Promise((r) => setTimeout(r, 50));
  child.kill();
  rmSync(tmp, { recursive: true, force: true });
  assert.ok(out.includes("fake-mcp"), `expected tools when Cursor imports the Claude plugin, got: ${out}`);
  assert.ok(!out.includes("unavailable"), `must not degrade, got: ${out}`);
});
