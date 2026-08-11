// test/idle-release.test.ts
//
// Idle-release: a resident topodb-mcp child holds redb's exclusive lock for
// the whole pi session, so nothing else (CLI, another agent) can touch the
// same db while pi is merely sitting idle. After TOPODB_IDLE_MS of quiet the
// extension must reap the child (the lazy-respawn path brings it back on the
// next call). Mirrors the sgh OnDemandBridge lease design (PR #65).
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { TopodbServer, idleMs } from "../src/server-handle.ts";

const env = (extra: Record<string, string> = {}) => ({
  ...process.env,
  TOPODB_DB: join(mkdtempSync(join(tmpdir(), "topodb-pi-idle-")), "m.redb"),
  ...extra,
});

test("idleMs: default 30s, explicit value, 0 disables, garbage falls back", () => {
  assert.equal(idleMs({}), 30_000);
  assert.equal(idleMs({ TOPODB_IDLE_MS: "150" }), 150);
  assert.equal(idleMs({ TOPODB_IDLE_MS: "0" }), 0);
  assert.equal(idleMs({ TOPODB_IDLE_MS: "soon" }), 30_000);
  assert.equal(idleMs({ TOPODB_IDLE_MS: "-5" }), 30_000);
});

test("server shuts down after TOPODB_IDLE_MS of quiet, then respawns on next call", async () => {
  const s = new TopodbServer(env({ TOPODB_IDLE_MS: "150" }));
  try {
    await s.call("create_memory", { content: "idle test" });
    assert.equal(s.running, true, "resident right after a call");
    await sleep(600);
    assert.equal(s.running, false, "reaped after the idle window");
    // Lazy respawn still works and the write persisted.
    const res: any = await s.call("search_memories", { query: "idle test", k: 5 });
    assert.equal(res.hits.length, 1);
    assert.equal(s.running, true, "resident again after the respawn call");
  } finally {
    s.shutdown();
  }
});

test("TOPODB_IDLE_MS=0 disables idle release", async () => {
  const s = new TopodbServer(env({ TOPODB_IDLE_MS: "0" }));
  try {
    await s.call("create_memory", { content: "resident test" });
    await sleep(400);
    assert.equal(s.running, true, "still resident: idle release disabled");
  } finally {
    s.shutdown();
  }
});

test("idle shutdown frees the redb lock for another process", async () => {
  const e = env({ TOPODB_IDLE_MS: "150" });
  const a = new TopodbServer(e);
  // Contender on the SAME db. Short lock-wait budget so a still-held lock
  // fails fast instead of eating the 3s server default.
  const b = new TopodbServer({ ...e, TOPODB_IDLE_MS: "0", TOPODB_LOCK_WAIT_MS: "1500" });
  try {
    await a.call("create_memory", { content: "handoff" });
    await sleep(600); // a's idle window elapses; child exits, lock frees
    const res: any = await b.call("search_memories", { query: "handoff", k: 5 });
    assert.equal(res.hits.length, 1, "second process read the first one's write");
  } finally {
    a.shutdown();
    b.shutdown();
  }
});

test("a cold spawn longer than the idle window is not killed mid-call", async () => {
  // Spawn alone takes ~200-450ms; if the timer armed at call START instead of
  // completion, a 75ms window would reap the child under the first call.
  const s = new TopodbServer(env({ TOPODB_IDLE_MS: "75" }));
  try {
    const res: any = await s.call("create_memory", { content: "slow start" });
    assert.ok(res.id, "call under a tiny idle window still completed");
  } finally {
    s.shutdown();
  }
});

test("list serves the cached tool list without respawning", async () => {
  const s = new TopodbServer(env({ TOPODB_IDLE_MS: "150" }));
  try {
    const first = await s.list();
    await sleep(600);
    assert.equal(s.running, false, "precondition: idled out");
    const again = await s.list();
    assert.deepEqual(again, first);
    assert.equal(s.running, false, "cached list answered without a respawn");
  } finally {
    s.shutdown();
  }
});
