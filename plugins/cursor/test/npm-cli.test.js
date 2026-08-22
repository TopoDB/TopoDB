import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { resolveNpmSpawn } from "../core/npm-cli.js";

test("posix prefers npm sitting next to node — Cursor MCP PATH often lacks /usr/local/bin", () => {
  const execPath = "/usr/local/bin/node";
  const sibling = path.posix.join("/usr/local/bin", "npm");
  const r = resolveNpmSpawn({
    execPath,
    platform: "darwin",
    existsSync: (p) => p === sibling,
  });
  assert.deepEqual(r, { command: sibling, args: [], shell: false });
});

test("posix uses unix-prefix npm-cli.js via the same node when there is no sibling npm", () => {
  const execPath = "/usr/local/bin/node";
  const cli = path.posix.join("/usr/local/bin", "..", "lib", "node_modules", "npm", "bin", "npm-cli.js");
  const r = resolveNpmSpawn({
    execPath,
    platform: "linux",
    existsSync: (p) => p === cli,
  });
  assert.deepEqual(r, { command: execPath, args: [cli], shell: false });
});

test("posix falls back to PATH npm last", () => {
  const r = resolveNpmSpawn({
    execPath: "/opt/custom/node",
    platform: "darwin",
    existsSync: () => false,
  });
  assert.deepEqual(r, { command: "npm", args: [], shell: false });
});

test("win32 uses npm-cli.js next to node.exe", () => {
  const execPath = "C:\\Program Files\\nodejs\\node.exe";
  const cli = path.win32.join("C:\\Program Files\\nodejs", "node_modules", "npm", "bin", "npm-cli.js");
  const r = resolveNpmSpawn({
    execPath,
    platform: "win32",
    existsSync: (p) => p === cli,
  });
  assert.deepEqual(r, { command: execPath, args: [cli], shell: false });
});
