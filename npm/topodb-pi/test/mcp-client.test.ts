// test/mcp-client.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { McpStdioClient } from "../src/mcp-client.ts";

const require = createRequire(import.meta.url);
const launcher = require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
const db = () => join(mkdtempSync(join(tmpdir(), "topodb-pi-")), "m.redb");

test("handshake + listTools reports 16 tools", async () => {
  const c = new McpStdioClient([launcher, "--db", db()]);
  await c.start();
  const tools = await c.listTools();
  assert.equal(tools.length, 16);
  assert.ok(tools.some((t) => t.name === "db_info"));
  c.stop();
});

test("callTool round-trips create_memory then search", async () => {
  const c = new McpStdioClient([launcher, "--db", db()]);
  await c.start();
  await c.callTool("create_memory", { content: "pi bridge smoke test" });
  const res: any = await c.callTool("search_memories", { query: "bridge", k: 5 });
  assert.equal(res.hits.length, 1);
  c.stop();
});

test("callTool surfaces engine errors as a rejection", async () => {
  const c = new McpStdioClient([launcher, "--db", db()]);
  await c.start();
  await assert.rejects(() => c.callTool("get_node", { id: "not-a-ulid" }));
  c.stop();
});

test("start() rejects with a timeout error when the server never responds", async () => {
  // A child that spawns fine but never reads stdin / never writes a response.
  const c = new McpStdioClient(["-e", "setInterval(() => {}, 1e9)"], {
    requestTimeoutMs: 200,
  });
  await assert.rejects(
    () => c.start(),
    /topodb-mcp request timed out after 200ms: initialize/,
  );
  c.stop();
});

test("spawn failure rejects start() instead of throwing an uncaught exception", async () => {
  const c = new McpStdioClient(["x"], {
    command: "C:/definitely/does/not/exist/node-binary.exe",
  });
  await assert.rejects(() => c.start());
  // No crash: reaching this line means the 'error' event was handled, not thrown.
  c.stop();
});
