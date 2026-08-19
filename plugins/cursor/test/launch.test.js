import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
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
