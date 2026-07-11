// test/server-handle.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TopodbServer } from "../src/server-handle.ts";

const env = () => ({
  ...process.env,
  TOPODB_DB: join(mkdtempSync(join(tmpdir(), "topodb-pi-sh-")), "m.redb"),
});

test("lazy: no child until first call, then list works", async () => {
  const s = new TopodbServer(env());
  const tools = await s.list();
  assert.equal(tools.length, 16);
  s.shutdown();
});

test("call respawns after shutdown", async () => {
  const e = env();
  const s = new TopodbServer(e);
  await s.call("create_memory", { content: "first" });
  s.shutdown();
  // Same db, new child: prior write persisted.
  const res: any = await s.call("search_memories", { query: "first", k: 5 });
  assert.equal(res.hits.length, 1);
  s.shutdown();
});

test("resolveLauncher finds the topodb-mcp launcher", () => {
  assert.match(TopodbServer.resolveLauncher(), /topodb-mcp\.js$/);
});
