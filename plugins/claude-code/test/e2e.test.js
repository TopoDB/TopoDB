import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { projectScopeId } from "../scope-id.js";

const require = createRequire(import.meta.url);

/** Minimal newline-delimited JSON-RPC client over the server's stdio. */
function client(args) {
  const bin = require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  const child = spawn(process.execPath, [bin, ...args], { stdio: ["pipe", "pipe", "inherit"] });
  const pending = new Map();
  let buf = "";
  child.stdout.on("data", (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      pending.get(msg.id)?.(msg);
    }
  });
  let id = 0;
  const rpc = (method, params) =>
    new Promise((res) => {
      const myId = ++id;
      pending.set(myId, res);
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
    });
  return { rpc, child };
}

test("the ULID this plugin derives is the ULID the Rust server reports back", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "topodb-plugin-"));
  const scope = projectScopeId(dir);
  const db = path.join(dir, "memory.redb");

  const { rpc, child } = client(["--db", db, "--scope", scope, "--read-scopes", `${scope},shared`]);
  await rpc("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "t", version: "0" },
  });

  const info = await rpc("tools/call", { name: "db_info", arguments: {} });
  child.kill();

  // The server PARSED our string with ScopeId::from_str and re-rendered it with
  // Display. Equality here is the cross-language guarantee.
  const body = JSON.stringify(info);
  assert.ok(!info.error, `server rejected the derived scope: ${body}`);
  assert.ok(
    body.includes(scope),
    `server did not echo our scope back.\n  derived: ${scope}\n  got: ${body}`,
  );
});
