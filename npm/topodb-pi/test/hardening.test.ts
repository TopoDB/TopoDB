// test/hardening.test.ts
//
// Failure-path hardening for idle-release (review findings on df3bcba):
// env propagation to the child, transport-tagged errors, awaitable exit,
// setTimeout-overflow clamp, start()-rejection cleanup, single-flight cold
// spawns, wedged-child reap, and toolCache refresh after a real respawn.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { McpStdioClient } from "../src/mcp-client.ts";
import { TopodbServer, idleMs } from "../src/server-handle.ts";

const tmp = (prefix: string) => mkdtempSync(join(tmpdir(), prefix));
const env = (extra: Record<string, string> = {}) => ({
  ...process.env,
  TOPODB_DB: join(tmp("topodb-pi-hard-"), "m.redb"),
  ...extra,
});

// A minimal line-oriented MCP stub: answers initialize, then behaves per mode.
//  - "env-echo": tools/list returns one tool named after $PROBE_TOOL_NAME
//  - "init-only": answers initialize, then never responds again (wedged child)
const stub = (mode: string) => [
  "-e",
  `
  const rl = require("node:readline").createInterface({ input: process.stdin });
  const send = (o) => process.stdout.write(JSON.stringify(o) + "\\n");
  rl.on("line", (l) => {
    let m; try { m = JSON.parse(l); } catch { return; }
    if (m.id === undefined) return;
    if (m.method === "initialize") return send({ jsonrpc: "2.0", id: m.id, result: {} });
    if ("${mode}" === "init-only") return; // wedged: alive but silent
    if (m.method === "tools/list")
      return send({ jsonrpc: "2.0", id: m.id, result: { tools: [{ name: process.env.PROBE_TOOL_NAME ?? "unset" }] } });
    send({ jsonrpc: "2.0", id: m.id, result: {} });
  });
  setInterval(() => {}, 1e9); // stay alive until stdin closes or killed
  rl.on("close", () => process.exit(0));
  `,
];
// Never responds at all — not even to initialize.
const hangStub = ["-e", "setInterval(() => {}, 1e9)"];

// ---------- McpStdioClient ----------

test("opts.env reaches the spawned child", async () => {
  const c = new McpStdioClient(stub("env-echo"), {
    requestTimeoutMs: 2000,
    env: { ...process.env, PROBE_TOOL_NAME: "pi-env-probe" },
  });
  try {
    await c.start();
    const tools = await c.listTools();
    assert.equal(tools[0]?.name, "pi-env-probe");
  } finally {
    c.stop();
  }
});

test("timeout and child-exit rejections carry transport=true; server-side tool errors do not", async () => {
  const hung = new McpStdioClient(hangStub, { requestTimeoutMs: 150 });
  try {
    await assert.rejects(
      () => hung.start(),
      (e: any) => e.transport === true,
      "timeout rejection must be transport-tagged",
    );
  } finally {
    hung.stop();
  }

  const { createRequire } = await import("node:module");
  const launcher = createRequire(import.meta.url).resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  const real = new McpStdioClient([launcher, "--db", join(tmp("topodb-pi-hard-"), "m.redb")]);
  try {
    await real.start();
    await assert.rejects(
      () => real.callTool("get_node", { id: "not-a-ulid" }),
      (e: any) => e.transport !== true,
      "an app-level error from a healthy child must NOT be transport-tagged",
    );
  } finally {
    real.stop();
  }
});

test("whenExited resolves after stop(), leaving running=false", async () => {
  const c = new McpStdioClient(stub("env-echo"), { requestTimeoutMs: 2000 });
  try {
    await c.start();
    assert.equal(c.running, true);
  } finally {
    c.stop();
  }
  assert.equal(typeof c.whenExited?.then, "function", "whenExited is a real promise, not undefined");
  await c.whenExited;
  assert.equal(c.running, false);
});

// ---------- idleMs ----------

test("idleMs clamps values beyond Node's setTimeout range instead of inverting them", () => {
  // Node clamps setTimeout delays > 2^31-1 to 1ms — a huge value meaning
  // "practically never" must not become "reap after every call".
  assert.equal(idleMs({ TOPODB_IDLE_MS: "9999999999" }), 2 ** 31 - 1);
});

// ---------- TopodbServer failure paths (stub-injected) ----------

test("a rejected start() does not leave an orphan child resident", async () => {
  const s = new TopodbServer(env(), {
    launcherArgs: hangStub,
    clientOpts: { requestTimeoutMs: 150 },
  });
  try {
    await assert.rejects(() => s.call("db_info", {}));
    assert.equal(s.running, false, "the hung child must be reaped, not left holding the lock");
  } finally {
    s.shutdown();
  }
});

test("concurrent cold callers share one start(): both see the start failure, not a bogus call timeout", async () => {
  const s = new TopodbServer(env(), {
    launcherArgs: hangStub,
    clientOpts: { requestTimeoutMs: 150 },
  });
  try {
    const [a, b] = await Promise.allSettled([s.call("db_info", {}), s.call("db_info", {})]);
    assert.equal(a.status, "rejected");
    assert.equal(b.status, "rejected");
    for (const r of [a, b] as PromiseRejectedResult[]) {
      assert.match(String(r.reason), /initialize/, "both callers get the shared handshake failure");
    }
  } finally {
    s.shutdown();
  }
});

test("a wedged child (transport timeout mid-call) is reaped instead of kept resident", async () => {
  const s = new TopodbServer(env(), {
    launcherArgs: stub("init-only"),
    clientOpts: { requestTimeoutMs: 200 },
  });
  try {
    await assert.rejects(() => s.call("db_info", {}));
    assert.equal(s.running, false, "wedged child must not stay resident holding the lock");
  } finally {
    s.shutdown();
  }
});

test("toolCache refreshes after a real respawn (still served while down)", async () => {
  // A stub whose tool list changes per spawn: generation counter in a file.
  const dir = tmp("topodb-pi-gen-");
  const counter = join(dir, "gen");
  writeFileSync(counter, "0");
  const genStub = [
    "-e",
    `
    const fs = require("node:fs");
    const gen = Number(fs.readFileSync(${JSON.stringify(counter)}, "utf8")) + 1;
    fs.writeFileSync(${JSON.stringify(counter)}, String(gen));
    const rl = require("node:readline").createInterface({ input: process.stdin });
    const send = (o) => process.stdout.write(JSON.stringify(o) + "\\n");
    rl.on("line", (l) => {
      let m; try { m = JSON.parse(l); } catch { return; }
      if (m.id === undefined) return;
      if (m.method === "tools/list")
        return send({ jsonrpc: "2.0", id: m.id, result: { tools: [{ name: "v" + gen }] } });
      send({ jsonrpc: "2.0", id: m.id, result: {} });
    });
    rl.on("close", () => process.exit(0));
    setInterval(() => {}, 1e9);
    `,
  ];
  const s = new TopodbServer(env(), { launcherArgs: genStub, clientOpts: { requestTimeoutMs: 2000 } });
  try {
    assert.equal((await s.list())[0]?.name, "v1");
    s.shutdown(); // stands in for an idle reap
    await s.whenReaped;
    assert.equal(s.running, false);
    assert.equal((await s.list())[0]?.name, "v1", "cache still served while down — no respawn for discovery");
    assert.equal(s.running, false);
    await s.call("anything", {}); // a real call respawns (generation 2)
    assert.equal((await s.list())[0]?.name, "v2", "respawn invalidated the cache");
  } finally {
    s.shutdown();
  }
});
