import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LAUNCH = path.join(HERE, "..", "launch.js");

test("launch.js resolves TOPODB_PLUGIN_DATA, logs it, and serves a degraded MCP server when bootstrap fails", async () => {
  const tmp = mkdtempSync(path.join(tmpdir(), "cursor-launch-"));
  const blocker = path.join(tmp, "file"); writeFileSync(blocker, "x");
  const dataDir = path.join(blocker, "data"); // under a FILE → mkdirSync throws → degraded path, no spawn
  const child = spawn(process.execPath, [LAUNCH], { env: { ...process.env, TOPODB_PLUGIN_DATA: dataDir, CURSOR_PROJECT_DIR: HERE }, stdio: ["pipe", "pipe", "pipe"] });
  let out = "", err = "";
  child.stdout.on("data", (d) => (out += d));
  child.stderr.on("data", (d) => (err += d));
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "t", version: "0" } } }) + "\n");
  const deadline = Date.now() + 15000;
  while (!out.includes('"id":1') && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
  child.kill();
  assert.ok(out.includes('"id":1'), `expected an initialize response from the degraded server, got: ${out}`);
  assert.match(err, /topodb: data dir .*\(override\)/);
  rmSync(tmp, { recursive: true, force: true });
});

test("when the daemon socket never binds, launch.js still serves real MCP tools over stdio", async () => {
  const tmp = mkdtempSync(path.join(tmpdir(), "cursor-stdio-fg-"));
  const dataDir = path.join(tmp, "data");
  mkdirSync(dataDir);
  const fake = path.join(tmp, "fake-mcp.mjs");
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
      } else if (msg.method === "tools/list") {
        process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { tools: [{ name: "search_memories", inputSchema: { type: "object" } }] } }) + "\\n");
      }
    }
  });
}
`,
  );
  chmodSync(fake, 0o755);
  const child = spawn(process.execPath, [LAUNCH], {
    env: {
      ...process.env,
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
  assert.ok(out.includes("fake-mcp"), `expected stdio fallback initialize, got: ${out}`);
  assert.ok(!out.includes("unavailable"), `must not advertise the degraded stub, got: ${out}`);
});
