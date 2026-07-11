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
