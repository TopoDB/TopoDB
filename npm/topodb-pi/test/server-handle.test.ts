// test/server-handle.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
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

test("creates the db's parent directory if it does not exist", async () => {
  // topodb-mcp creates the .redb file but treats a missing parent directory as
  // a startup error. The default db path (.topodb/memory.redb) has a parent
  // that won't exist in a fresh project, so the extension must create it.
  const base = mkdtempSync(join(tmpdir(), "topodb-pi-mkdir-"));
  const db = join(base, "nested", "deep", "memory.redb"); // parents don't exist
  assert.equal(existsSync(dirname(db)), false, "precondition: parent absent");

  const s = new TopodbServer({ ...process.env, TOPODB_DB: db });
  const tools = await s.list(); // before the fix this rejects (server exits)
  assert.equal(tools.length, 16);
  assert.ok(existsSync(dirname(db)), "parent directory was created");
  assert.ok(existsSync(db), "db file was created");
  s.shutdown();
});
