import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { projectScopeId } from "../scope-id.js";

const require = createRequire(import.meta.url);

const RPC_TIMEOUT_MS = 5000;

/** Minimal newline-delimited JSON-RPC client over the server's stdio. */
function client(args) {
  const bin = require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  const child = spawn(process.execPath, [bin, ...args], { stdio: ["pipe", "pipe", "inherit"] });
  const pending = new Map();
  let buf = "";
  let dead = null; // set once the child exits or errors, so late rpc() calls fail fast too

  const failAllPending = (err) => {
    dead = err;
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(err);
    }
    pending.clear();
  };

  child.stdout.on("data", (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      const entry = pending.get(msg.id);
      if (entry) {
        clearTimeout(entry.timer);
        pending.delete(msg.id);
        entry.resolve(msg);
      }
    }
  });
  child.on("exit", (code, signal) => {
    failAllPending(new Error(`topodb-mcp exited before responding (code=${code}, signal=${signal})`));
  });
  child.on("error", (err) => {
    failAllPending(new Error(`topodb-mcp failed to start: ${err.message}`));
  });

  let id = 0;
  const rpc = (method, params) =>
    new Promise((resolve, reject) => {
      if (dead) {
        reject(dead);
        return;
      }
      const myId = ++id;
      const timer = setTimeout(() => {
        pending.delete(myId);
        reject(new Error(`rpc "${method}" timed out after ${RPC_TIMEOUT_MS}ms waiting for a response`));
      }, RPC_TIMEOUT_MS);
      pending.set(myId, { resolve, reject, timer });
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
    });
  return { rpc, child };
}

test("the ULID this plugin derives is the ULID the Rust server reports back", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "topodb-plugin-"));
  try {
    const scope = projectScopeId(dir);
    const db = path.join(dir, "memory.redb");

    const { rpc, child } = client(["--db", db, "--scope", scope, "--read-scopes", `${scope},shared`]);
    try {
      await rpc("initialize", {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "t", version: "0" },
      });

      const info = await rpc("tools/call", { name: "db_info", arguments: {} });

      // The server PARSED our string with ScopeId::from_str and re-rendered it with
      // Display. Equality here is the cross-language guarantee.
      const body = JSON.stringify(info);
      assert.ok(!info.error, `server rejected the derived scope: ${body}`);
      assert.ok(
        body.includes(scope),
        `server did not echo our scope back.\n  derived: ${scope}\n  got: ${body}`,
      );
    } finally {
      child.kill();
    }
  } finally {
    // On Windows, child.kill() is TerminateProcess, which is asynchronous —
    // the OS can still be releasing redb's handle on memory.redb when this
    // rmSync runs, racing an EBUSY/EPERM. maxRetries + retryDelay give the
    // handle time to actually let go instead of flaking the cleanup.
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
