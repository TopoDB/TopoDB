// Cross-process concurrency tests for the plugin broker.
//
// THE MISTAKE THIS FILE EXISTS TO NOT REPEAT: every earlier test in this repo
// ran Claude Code sessions SEQUENTIALLY, and a sequential test cannot see the
// original bug -- redb's process-exclusive lock only bites when two sessions
// open the SAME database at (near) the SAME instant. An in-process test, or a
// test that awaits session A before starting session B, would pass while the
// real bug persists. Every test below spawns REAL, CONCURRENT `launch.js`
// child processes against ONE `CLAUDE_PLUGIN_DATA` (hence one memory.redb).
//
// Every rpc() call has a timeout that REJECTS (never hangs), and every
// spawned child is killed in a `finally` -- a test that hangs instead of
// failing is worse than no test at all.
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import net from "node:net";
import path from "node:path";
import { lineReader, socketPathFor, helloFrame } from "../ipc.js";
import { projectScopeId } from "../scope-id.js";
import { rmWithGraceSync } from "./fsgrace.js";
import { SERVER_VERSION } from "../server-args.js";

const require = createRequire(import.meta.url);
// The real launcher's own platform-key logic, not a copy of it. A second copy of
// this table is exactly the kind of drift that produced the bug below.
const { binaryFileName } = require("@topodb/topodb-mcp/bin/topodb-mcp.js");
const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.join(HERE, "..");
const LAUNCH_JS = path.join(PLUGIN_ROOT, "launch.js");
const BROKER_JS = path.join(PLUGIN_ROOT, "broker.js");

const DEFAULT_RPC_TIMEOUT_MS = 8000;

// --- test plumbing -----------------------------------------------------

/** A temp CLAUDE_PLUGIN_DATA dir with the server pre-linked in, so
 * resolveServer() finds a matching version and skips a real (slow,
 * network-dependent) `npm install`. A junction needs no elevated privilege
 * on Windows, unlike a symlink. */
function mkDataDir(prefix) {
  const dir = mkdtempSync(path.join(tmpdir(), prefix));
  mkdirSync(path.join(dir, "node_modules"), { recursive: true });
  symlinkSync(
    path.join(PLUGIN_ROOT, "node_modules", "@topodb"),
    path.join(dir, "node_modules", "@topodb"),
    process.platform === "win32" ? "junction" : "dir",
  );
  return dir;
}

/** Like `mkDataDir`, but COPIES the server into the data dir instead of
 * junctioning it. A test that has to DELETE part of the installed tree cannot
 * use the junction — it would delete out of this repo's own node_modules and
 * poison every later test in the run. */
function mkRealDataDir(prefix) {
  const dir = mkdtempSync(path.join(tmpdir(), prefix));
  cpSync(path.join(PLUGIN_ROOT, "node_modules", "@topodb"), path.join(dir, "node_modules", "@topodb"), {
    recursive: true,
  });
  return dir;
}

/** The platform package THIS host needs — the one the shim will look for. */
function platformPkgDir(dataDir) {
  return path.join(dataDir, "node_modules", "@topodb", `topodb-mcp-${process.platform}-${process.arch}`);
}

function rmDir(dir) {
  // Windows kill() is asynchronous (TerminateProcess) and won't remove a
  // directory while the just-killed broker (or its topodb-mcp child) still
  // holds a handle on memory.redb / broker.log — rmdir then throws ENOTEMPTY
  // (or EBUSY/EPERM), failing the test in teardown, not on any assertion. This
  // file's own retry budget (10×100ms ≈ 5.5s) was too short and flaked; use the
  // shared grace helper (30×1000ms, and it names what is still held if it ever
  // does give up). Sync variant because these teardowns run in sync `finally`s.
  rmWithGraceSync(dir);
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/** Minimal newline-delimited JSON-RPC client over a child process's stdio,
 * with a timeout on every request that REJECTS rather than hanging. */
function spawnRpcClient(command, args, env = {}) {
  const child = spawn(command, args, {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, ...env },
  });

  const pending = new Map();
  let dead = null;
  let stderrBuf = "";
  child.stderr.on("data", (d) => (stderrBuf += d));

  const failAll = (err) => {
    dead = err;
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(err);
    }
    pending.clear();
  };

  child.stdout.on(
    "data",
    lineReader((line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      const entry = pending.get(msg.id);
      if (entry) {
        clearTimeout(entry.timer);
        pending.delete(msg.id);
        entry.resolve(msg);
      }
    }),
  );
  child.on("exit", (code, signal) => {
    failAll(new Error(`process exited (code=${code}, signal=${signal}) before responding; stderr: ${stderrBuf.slice(-2000)}`));
  });
  child.on("error", (err) => {
    failAll(new Error(`process failed to start: ${err.message}`));
  });

  let autoId = 0;
  const rpc = (method, params, opts = {}) =>
    new Promise((resolve, reject) => {
      if (dead) {
        reject(dead);
        return;
      }
      const myId = opts.id ?? ++autoId;
      const timeoutMs = opts.timeoutMs ?? DEFAULT_RPC_TIMEOUT_MS;
      const timer = setTimeout(() => {
        pending.delete(myId);
        reject(new Error(`rpc "${method}" (id=${myId}) timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      pending.set(myId, { resolve, reject, timer });
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
    });

  const notify = (method, params) => {
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  };

  return { rpc, notify, child, stderr: () => stderrBuf };
}

/** Spawns `launch.js` -- a real Claude Code session's shim -- against a given
 * (shared) plugin data dir and (per-session) project dir. */
function launchSession({ dataDir, projectDir, env = {} }) {
  return spawnRpcClient(process.execPath, [LAUNCH_JS], {
    CLAUDE_PLUGIN_DATA: dataDir,
    CLAUDE_PROJECT_DIR: projectDir,
    // Keep every broker THIS suite spawns short-lived. Without this it
    // inherits the production 60s idle timeout and lingers as an orphan
    // process for up to a minute after each test finishes.
    TOPODB_BROKER_IDLE_MS: "5000",
    ...env,
  });
}

async function connectAndInit({ dataDir, projectDir, env, initTimeoutMs }) {
  const session = launchSession({ dataDir, projectDir, env });
  const initMsg = await session.rpc(
    "initialize",
    { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "broker-test", version: "0" } },
    { timeoutMs: initTimeoutMs ?? DEFAULT_RPC_TIMEOUT_MS },
  );
  if (initMsg.error) {
    throw new Error(`initialize failed: ${JSON.stringify(initMsg.error)}`);
  }
  session.notify("notifications/initialized");
  return session;
}

/* Concurrent connectAndInit with leak-proof failure: if one session fails
 * to initialize, the sibling that SUCCEEDED must still land in `sessions`
 * so the test's killAll(finally) reaches it. A bare Promise.all rejection
 * strands the winner's shim alive — its open stdio pipes keep this test
 * process's event loop spinning after the failed test, and node --test
 * then waits on the file forever (the intermittent multi-hour CI hang). */
async function connectPairTracked(sessions, specA, specB) {
  const settled = await Promise.allSettled([connectAndInit(specA), connectAndInit(specB)]);
  for (const r of settled) if (r.status === "fulfilled") sessions.push(r.value);
  const failed = settled.find((r) => r.status === "rejected");
  if (failed) throw failed.reason;
  return settled.map((r) => r.value);
}

function spawnRawServer(dbPath, scope) {
  const bin = require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js");
  return spawnRpcClient(process.execPath, [bin, "--db", dbPath, "--scope", scope, "--read-scopes", `${scope},shared`]);
}

function killAll(sessions) {
  for (const s of sessions) {
    try {
      s.child.kill();
    } catch {}
  }
}

function killAndWaitForExit(session) {
  return new Promise((resolve) => {
    if (session.child.exitCode !== null || session.child.signalCode !== null) {
      resolve();
      return;
    }
    session.child.once("exit", () => resolve());
    session.child.kill();
  });
}

// --- C2 (notifications/cancelled id-translation) plumbing ----------------
//
// Pinning C2 needs an EXACT id collision: session A's own client-chosen id
// must equal the broker's upstream id for session B's in-flight request. Real
// timing can't guarantee that deterministically, so this fake server never
// answers a "hold" call until told to, and echoes back the upstream id the
// broker assigned each forwarded request (a notification real servers never
// send -- it exists ONLY so the test can construct the collision on purpose
// instead of hoping a race lands right).
const FAKE_CANCEL_SERVER_SRC = `
import { createInterface } from "node:readline";
const rl = createInterface({ input: process.stdin, terminal: false });
const held = new Map();
const out = (o) => process.stdout.write(JSON.stringify(o) + "\\n");
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.method === "initialize") {
    out({ jsonrpc: "2.0", id: msg.id, result: {
      protocolVersion: "2024-11-05", capabilities: { tools: {} },
      serverInfo: { name: "fake-cancel-server", version: "0" },
    }});
    return;
  }
  if (msg.method === "notifications/initialized") return;
  if (msg.method === "notifications/cancelled") {
    const rid = msg.params && msg.params.requestId;
    if (held.has(rid)) {
      held.delete(rid);
      out({ jsonrpc: "2.0", id: rid, error: { code: -32800, message: "cancelled" } });
    }
    return;
  }
  if (msg.method === "tools/call" && msg.id !== undefined) {
    out({ jsonrpc: "2.0", method: "notifications/test-received", params: { upstreamId: msg.id, name: msg.params && msg.params.name } });
    const name = msg.params && msg.params.name;
    if (name === "hold") { held.set(msg.id, true); return; }
    if (name === "count_held") {
      out({ jsonrpc: "2.0", id: msg.id, result: { structuredContent: { heldIds: Array.from(held.keys()) } } });
      return;
    }
    if (name === "flush") {
      for (const id of held.keys()) out({ jsonrpc: "2.0", id, result: { structuredContent: { status: "completed" } } });
      held.clear();
      out({ jsonrpc: "2.0", id: msg.id, result: { structuredContent: { status: "flushed" } } });
      return;
    }
    out({ jsonrpc: "2.0", id: msg.id, result: { structuredContent: { status: "completed" } } });
  }
});
`;

/** A CLAUDE_PLUGIN_DATA dir wired to the fake cancel-tracking server above
 * instead of the real topodb-mcp, so broker.js's require.resolve finds it at
 * the exact subpath it expects. */
function mkFakeCancelServerDataDir(prefix) {
  const dir = mkdtempSync(path.join(tmpdir(), prefix));
  const pkgDir = path.join(dir, "node_modules", "@topodb", "topodb-mcp");
  mkdirSync(path.join(pkgDir, "bin"), { recursive: true });
  writeFileSync(path.join(pkgDir, "package.json"), JSON.stringify({ name: "@topodb/topodb-mcp", version: "0.0.5", type: "module" }));
  writeFileSync(path.join(pkgDir, "bin", "topodb-mcp.js"), FAKE_CANCEL_SERVER_SRC);
  return dir;
}

/** Newline-delimited JSON-RPC client over a raw socket -- connects DIRECTLY
 * to a broker's socket, bypassing launch.js entirely, so a test can act as
 * its own MCP session and choose ITS OWN client-space ids explicitly (needed
 * to construct the exact id collision C2 is about). */
/** A raw client on the broker's socket, standing in for a session's shim.
 *
 * It MUST open with a hello frame, because that is what a real shim does
 * (`launch.js`'s `relay`) and the broker now refuses to forward a request from a
 * connection whose scope it does not know — it would otherwise have to run that
 * request under whatever scope the server happened to be started with, which is
 * precisely the scope-bleed bug. A test double that skipped the hello would be
 * testing a client that cannot exist. */
function socketRpcClient(sock, { scope = "t6scope", readScopes = ["t6scope", "shared"] } = {}) {
  const conn = net.connect(sock);
  conn.write(helloFrame({ scope, readScopes }));
  const pending = new Map();
  const notifications = [];
  const waiters = [];
  let dead = null;

  const failAll = (err) => {
    dead = err;
    for (const { reject, timer } of pending.values()) {
      clearTimeout(timer);
      reject(err);
    }
    pending.clear();
    for (const w of waiters.splice(0)) {
      clearTimeout(w.timer);
      w.reject(err);
    }
  };

  conn.on(
    "data",
    lineReader((line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.id !== undefined && pending.has(msg.id)) {
        const entry = pending.get(msg.id);
        clearTimeout(entry.timer);
        pending.delete(msg.id);
        entry.resolve(msg);
        return;
      }
      notifications.push(msg);
      for (let i = waiters.length - 1; i >= 0; i--) {
        if (waiters[i].predicate(msg)) {
          const w = waiters.splice(i, 1)[0];
          clearTimeout(w.timer);
          w.resolve(msg);
        }
      }
    }),
  );
  conn.on("close", () => failAll(new Error("socket closed before responding")));
  conn.on("error", (err) => failAll(new Error(`socket error: ${err.message}`)));

  const rpc = (method, params, opts = {}) =>
    new Promise((resolve, reject) => {
      if (dead) {
        reject(dead);
        return;
      }
      const myId = opts.id;
      const timeoutMs = opts.timeoutMs ?? DEFAULT_RPC_TIMEOUT_MS;
      const timer = setTimeout(() => {
        pending.delete(myId);
        reject(new Error(`rpc "${method}" (id=${myId}) timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      pending.set(myId, { resolve, reject, timer });
      conn.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
    });

  const notify = (method, params) => {
    conn.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  };

  const waitForNotification = (predicate, timeoutMs = DEFAULT_RPC_TIMEOUT_MS) =>
    new Promise((resolve, reject) => {
      const already = notifications.find(predicate);
      if (already) {
        resolve(already);
        return;
      }
      const timer = setTimeout(() => {
        const idx = waiters.findIndex((w) => w.resolve === resolve);
        if (idx >= 0) waiters.splice(idx, 1);
        reject(new Error(`timed out after ${timeoutMs}ms waiting for a matching notification`));
      }, timeoutMs);
      waiters.push({ predicate, resolve, reject, timer });
    });

  return { conn, rpc, notify, waitForNotification, close: () => conn.destroy() };
}

async function connectSocketWithRetry(sock, { retries = 50, intervalMs = 100 } = {}) {
  for (let i = 0; i < retries; i++) {
    const ok = await new Promise((res) => {
      const c = net.connect(sock);
      c.on("connect", () => {
        c.destroy();
        res(true);
      });
      c.on("error", () => res(false));
    });
    if (ok) return;
    await sleep(intervalMs);
  }
  throw new Error(`could not connect to ${sock} after ${retries * intervalMs}ms`);
}

// --- tests ---------------------------------------------------------------

test("two_concurrent_sessions_both_get_memory", async () => {
  const dataDir = mkDataDir("topodb-t1-data-");
  const projA = mkdtempSync(path.join(tmpdir(), "topodb-t1-projA-"));
  const projB = mkdtempSync(path.join(tmpdir(), "topodb-t1-projB-"));
  const sessions = [];
  try {
    // THE regression test. Both shims spawned at the same instant against the
    // SAME CLAUDE_PLUGIN_DATA (hence the same memory.redb). Before the broker,
    // the second process to reach redb's exclusive lock died with
    // Storage(DatabaseAlreadyOpen) -- silently, while the skill still told the
    // agent to call search_memories.
    const [a, b] = await connectPairTracked(
      sessions,
      { dataDir, projectDir: projA },
      { dataDir, projectDir: projB },
    );

    const [infoA, infoB] = await Promise.all([
      a.rpc("tools/call", { name: "db_info", arguments: {} }),
      b.rpc("tools/call", { name: "db_info", arguments: {} }),
    ]);

    assert.ok(!infoA.error, `session A's db_info errored: ${JSON.stringify(infoA)}`);
    assert.ok(!infoB.error, `session B's db_info errored: ${JSON.stringify(infoB)}`);

    const pathA = infoA.result.structuredContent.path;
    const pathB = infoB.result.structuredContent.path;
    // Proves they truly shared ONE database rather than quietly getting their
    // own -- the exact failure mode this broker exists to rule out.
    assert.equal(pathA, pathB, `sessions reported different db paths: ${pathA} vs ${pathB}`);
    assert.equal(pathA, path.join(dataDir, "memory.redb"));
  } finally {
    killAll(sessions);
    rmDir(dataDir);
    rmDir(projA);
    rmDir(projB);
  }
});

test("answers_do_not_cross_between_sessions", async () => {
  const dataDir = mkDataDir("topodb-t2-data-");
  const projA = mkdtempSync(path.join(tmpdir(), "topodb-t2-projA-"));
  const projB = mkdtempSync(path.join(tmpdir(), "topodb-t2-projB-"));
  const sessions = [];
  try {
    const [a, b] = await connectPairTracked(
      sessions,
      { dataDir, projectDir: projA },
      { dataDir, projectDir: projB },
    );

    const markerA = `marker-A-${randomUUID()}`;
    const markerB = `marker-B-${randomUUID()}`;

    // Both clients use the SAME JSON-RPC id (1) for DIFFERENT requests, sent
    // at the same instant. This pins the broker's id-namespacing: if it
    // forwarded ids unrewritten, session B could receive session A's answer
    // -- silent, plausible, and catastrophic for a memory tool.
    const [resA, resB] = await Promise.all([
      a.rpc("tools/call", { name: "create_memory", arguments: { content: markerA } }, { id: 1 }),
      b.rpc("tools/call", { name: "create_memory", arguments: { content: markerB } }, { id: 1 }),
    ]);

    assert.ok(!resA.error, `session A's create_memory errored: ${JSON.stringify(resA)}`);
    assert.ok(!resB.error, `session B's create_memory errored: ${JSON.stringify(resB)}`);
    assert.equal(resA.id, 1);
    assert.equal(resB.id, 1);

    const idA = resA.result.structuredContent.id;
    const idB = resB.result.structuredContent.id;
    assert.notEqual(idA, idB, "two distinct writes must not collapse to one node id");

    // {id} alone can't reveal a swap -- both are plausible-looking ULIDs.
    // Fetch each node back through its OWN session and check it holds the
    // marker THAT session wrote; a crossed answer would show up right here.
    const [nodeA, nodeB] = await Promise.all([
      a.rpc("tools/call", { name: "get_node", arguments: { id: idA } }),
      b.rpc("tools/call", { name: "get_node", arguments: { id: idB } }),
    ]);
    assert.ok(!nodeA.error, JSON.stringify(nodeA));
    assert.ok(!nodeB.error, JSON.stringify(nodeB));

    const contentA = nodeA.result.structuredContent.node.props.content;
    const contentB = nodeB.result.structuredContent.node.props.content;
    assert.equal(contentA, markerA, "session A's node did not contain session A's content");
    assert.equal(contentB, markerB, "session B's node did not contain session B's content");
  } finally {
    killAll(sessions);
    rmDir(dataDir);
    rmDir(projA);
    rmDir(projB);
  }
});

test("startup_race_elects_one_broker", async () => {
  const dataDir = mkDataDir("topodb-t3-data-");
  const projDirs = Array.from({ length: 5 }, (_, i) => mkdtempSync(path.join(tmpdir(), `topodb-t3-proj${i}-`)));
  const sessions = [];
  try {
    // Five shims race a COLD db at once. Only one wins redb's exclusive lock
    // and becomes the broker (see broker.js's election comment); the other
    // four's brokers exit with DatabaseAlreadyOpen without ever binding the
    // socket, and their shims connect to the winner instead.
    const results = await Promise.all(projDirs.map((projectDir) => connectAndInit({ dataDir, projectDir })));
    sessions.push(...results);

    const infos = await Promise.all(sessions.map((s) => s.rpc("tools/call", { name: "db_info", arguments: {} })));
    infos.forEach((info, i) => assert.ok(!info.error, `session ${i}'s db_info errored: ${JSON.stringify(info)}`));

    const paths = infos.map((info) => info.result.structuredContent.path);
    assert.ok(
      paths.every((p) => p === paths[0]),
      `expected all five sessions to share one db path, got: ${JSON.stringify(paths)}`,
    );
  } finally {
    killAll(sessions);
    rmDir(dataDir);
    for (const d of projDirs) rmDir(d);
  }
});

test("each_session_writes_to_its_own_project_scope", async () => {
  const dataDir = mkDataDir("topodb-t3b-data-");
  const projA = mkdtempSync(path.join(tmpdir(), "topodb-t3b-projA-"));
  const projB = mkdtempSync(path.join(tmpdir(), "topodb-t3b-projB-"));
  const sessions = [];
  try {
    // THE SCOPE-BLEED REGRESSION TEST. The tests above already prove two
    // projects SHARE one database and one broker -- which is the whole point.
    // What none of them assert is which SCOPE each session lands in, and that
    // omission is exactly why the bug shipped: `serverArgs()` bakes
    // `--scope <ULID(projectDir)>` into argv, `broker.js` spawns ONE
    // topodb-mcp with whichever session's argv got there first, and
    // `socketPathFor(dbPath)` keys the broker on the DB PATH ALONE -- which is
    // identical for every project. So session B connects to A's broker and
    // silently inherits A's scope.
    //
    // t2 (answers_do_not_cross_between_sessions) PASSES while this is broken:
    // it writes a marker per session and reads each back, which works fine
    // when both sessions sit in the same scope. Sharing a scope is invisible
    // to it. Only asserting the scope itself can see this.
    const [a, b] = await connectPairTracked(
      sessions,
      { dataDir, projectDir: projA },
      { dataDir, projectDir: projB },
    );

    const [infoA, infoB] = await Promise.all([
      a.rpc("tools/call", { name: "db_info", arguments: {} }),
      b.rpc("tools/call", { name: "db_info", arguments: {} }),
    ]);
    assert.ok(!infoA.error, `session A's db_info errored: ${JSON.stringify(infoA)}`);
    assert.ok(!infoB.error, `session B's db_info errored: ${JSON.stringify(infoB)}`);

    // Derived independently, from each project's own path -- the value each
    // session is SUPPOSED to be writing under.
    const wantA = projectScopeId(projA);
    const wantB = projectScopeId(projB);
    assert.notEqual(wantA, wantB, "two different project dirs must derive two different scopes");

    assert.equal(
      infoA.result.structuredContent.default_scope,
      wantA,
      "session A's write scope is not its own project's scope",
    );
    assert.equal(
      infoB.result.structuredContent.default_scope,
      wantB,
      `session B inherited another project's scope (scope bleed): expected ${wantB}, got ${infoB.result.structuredContent.default_scope}`,
    );
  } finally {
    killAll(sessions);
    rmDir(dataDir);
    rmDir(projA);
    rmDir(projB);
  }
});

test("one_project_cannot_read_another_projects_memory", async () => {
  const dataDir = mkDataDir("topodb-t3c-data-");
  const projA = mkdtempSync(path.join(tmpdir(), "topodb-t3c-projA-"));
  const projB = mkdtempSync(path.join(tmpdir(), "topodb-t3c-projB-"));
  const sessions = [];
  try {
    // The consequence that actually matters to a user. The scope assertion
    // above is the mechanism; this is the harm: with the scopes collapsed,
    // project B's agent can recall project A's private memories. The README
    // promises the opposite ("reads span this project's scope plus shared").
    const [a, b] = await connectPairTracked(
      sessions,
      { dataDir, projectDir: projA },
      { dataDir, projectDir: projB },
    );

    const secretOfA = `project-A-private-${randomUUID()}`;
    const wrote = await a.rpc("tools/call", {
      name: "create_memory",
      arguments: { content: secretOfA },
    });
    assert.ok(!wrote.error, `session A's create_memory errored: ${JSON.stringify(wrote)}`);

    // B searches its OWN default read set ({B's project} + shared). A's memory
    // was written to A's project scope, so it must not appear.
    const found = await b.rpc("tools/call", {
      name: "search_memories",
      arguments: { query: secretOfA },
    });
    assert.ok(!found.error, `session B's search_memories errored: ${JSON.stringify(found)}`);

    const body = JSON.stringify(found.result);
    assert.ok(
      !body.includes(secretOfA),
      `project B recalled project A's private memory (scope bleed).\n  secret: ${secretOfA}\n  B saw: ${body.slice(0, 800)}`,
    );

    // Positive control: without this, the assertion above would also "pass" if
    // search_memories were simply broken and returned nothing for everyone.
    const ownHit = await a.rpc("tools/call", {
      name: "search_memories",
      arguments: { query: secretOfA },
    });
    assert.ok(!ownHit.error, `session A's search_memories errored: ${JSON.stringify(ownHit)}`);
    assert.ok(
      JSON.stringify(ownHit.result).includes(secretOfA),
      "session A could not recall its OWN memory -- the negative assertion above proves nothing",
    );
  } finally {
    killAll(sessions);
    rmDir(dataDir);
    rmDir(projA);
    rmDir(projB);
  }
});

test("repairs an install whose platform binary is missing", async () => {
  // THE GHOST-BINARY BUG, reproduced. On a real Windows install, npm resolved
  // the WRONG platform's optional dependency (`topodb-mcp-linux-x64` on a win32
  // host), so this host's platform package was simply absent — while
  // `@topodb/topodb-mcp` itself sat at exactly the pinned SERVER_VERSION.
  //
  // Every check in resolveServer passed, because every one of them looked at the
  // JS shim. None looked at the BINARY. `require.resolve` then walked up out of
  // the data dir, found a stale topodb-mcp-win32-x64@0.0.3 elsewhere on the
  // machine, and ran it: a server two format generations old, launched by a
  // plugin that believed it was on 0.0.7, with no error anywhere.
  //
  // This fixture is that exact state: correct shim, missing platform package.
  const dataDir = mkRealDataDir("topodb-t7-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t7-proj-"));
  const pkg = platformPkgDir(dataDir);
  const sessions = [];
  try {
    // Positive control. If the fixture never had the platform package, its
    // absence below would prove nothing and the repair would have nothing to
    // repair — the test would "pass" while testing air.
    assert.ok(existsSync(pkg), `fixture is broken: ${pkg} must exist BEFORE we remove it`);
    rmSync(pkg, { recursive: true, force: true });
    assert.ok(!existsSync(pkg), "fixture: platform package should now be gone");

    // The shim must still be at the pinned version — that is what made the old
    // code believe the install was healthy.
    const shimPkg = path.join(dataDir, "node_modules", "@topodb", "topodb-mcp", "package.json");
    assert.equal(JSON.parse(readFileSync(shimPkg, "utf8")).version, SERVER_VERSION);

    // Generous timeouts: the repair is a REAL `npm install` of the platform
    // package (this is the one test in the suite that genuinely needs the
    // network — the whole point is the install path the others skip).
    //
    // A short idle window because, unlike every other test here, this data dir
    // holds a real COPY of the server rather than a junction to the repo's — so
    // the topodb-mcp.exe the broker is running lives INSIDE the directory the
    // teardown has to delete, and Windows will not delete a running executable.
    // The broker has to exit before cleanup can succeed.
    const session = await connectAndInit({
      dataDir,
      projectDir: proj,
      env: { TOPODB_BROKER_IDLE_MS: "500" },
      initTimeoutMs: 120_000,
    });
    sessions.push(session);

    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} }, { timeoutMs: 60_000 });
    assert.ok(!info.error, `server never came up after repair: ${JSON.stringify(info)}`);
    assert.equal(info.result.structuredContent.path, path.join(dataDir, "memory.redb"));

    // The repair must have put the RIGHT package in the data dir — not merely
    // found some binary somewhere. Resolving a binary is exactly what the old
    // code did successfully, out of a directory it did not own.
    assert.ok(existsSync(pkg), `launch.js did not reinstall the platform package at ${pkg}`);
    assert.equal(
      JSON.parse(readFileSync(path.join(pkg, "package.json"), "utf8")).version,
      SERVER_VERSION,
      "the repaired platform package must be the pinned server version, not whatever npm felt like",
    );
  } finally {
    killAll(sessions);
    // Wait out the (shortened) idle window so the broker exits and releases the
    // running topodb-mcp.exe that lives inside dataDir — see above.
    await sleep(3000);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("never runs a stale binary resolved from outside its own data dir", async () => {
  // The bug AS IT ACTUALLY HAPPENED, end to end. Missing platform package in the
  // data dir + a stale copy of that package in an ANCESTOR node_modules.
  // `require.resolve` walks up, finds the stale one, and returns it. Resolution
  // SUCCEEDS -- so the shim's "not installed" error never fires, no repair is
  // attempted, and the broker executes a topodb-mcp@0.0.3 while SERVER_VERSION,
  // the installed shim, and npm all say 0.0.7. Nothing anywhere reports a
  // problem; the tools simply never appear.
  //
  // The plugin must not depend on a NEW shim to catch this: the installs that
  // have the bug are, by definition, the ones running an older shim. Ownership
  // is checkable here -- a binary we own lives under our own data dir.
  // `realpathSync` matters here, it is not tidying: on macOS `tmpdir()` is
  // `/var/folders/...`, a symlink to `/private/var/folders/...`. Node's
  // `require.resolve` returns the REAL path, so the fixture-sanity assertion
  // below would compare `/private/var/...` against `/var/...` and fail —
  // reporting "fixture is inert" when the fixture is fine. Resolving the temp
  // roots once means every path derived from them is already canonical.
  const root = realpathSync(mkdtempSync(path.join(tmpdir(), "topodb-t8-root-")));
  const proj = realpathSync(mkdtempSync(path.join(tmpdir(), "topodb-t8-proj-")));
  const sessions = [];
  try {
    // dataDir is nested one level down, so `root/node_modules` is an ANCESTOR of
    // it -- exactly the position the stale package occupied on the real machine.
    const dataDir = path.join(root, "data");
    mkdirSync(dataDir, { recursive: true });
    cpSync(path.join(PLUGIN_ROOT, "node_modules", "@topodb"), path.join(dataDir, "node_modules", "@topodb"), {
      recursive: true,
    });

    const key = `${process.platform}-${process.arch}`;
    const ours = path.join(dataDir, "node_modules", "@topodb", `topodb-mcp-${key}`);
    assert.ok(existsSync(ours), `fixture: ${ours} must exist before we remove it`);
    rmSync(ours, { recursive: true, force: true });

    // THE GHOST: a stale platform package where the tree-walk will find it.
    const ghost = path.join(root, "node_modules", "@topodb", `topodb-mcp-${key}`);
    mkdirSync(ghost, { recursive: true });
    writeFileSync(
      path.join(ghost, "package.json"),
      JSON.stringify({ name: `@topodb/topodb-mcp-${key}`, version: "0.0.3" }),
    );
    // Deliberately NOT a working server. If the plugin ever executes this, the
    // test fails -- which is the entire point. On the real machine this was a
    // genuine 0.0.3 server, which is why it failed silently instead of loudly.
    writeFileSync(path.join(ghost, binaryFileName(process.platform)), "not a real server\n");

    // Sanity: the ghost really is reachable by Node's resolution from the shim.
    // Without this, a passing test might just mean the walk-up never found it.
    const shim = path.join(dataDir, "node_modules", "@topodb", "topodb-mcp", "bin", "topodb-mcp.js");
    const resolvedGhost = createRequire(shim).resolve(
      `@topodb/topodb-mcp-${key}/${binaryFileName(process.platform)}`,
    );
    assert.equal(
      path.resolve(resolvedGhost),
      path.resolve(path.join(ghost, binaryFileName(process.platform))),
      "fixture is inert: Node did not actually resolve the ghost, so this test proves nothing",
    );

    const session = await connectAndInit({
      dataDir,
      projectDir: proj,
      env: { TOPODB_BROKER_IDLE_MS: "500" },
      initTimeoutMs: 120_000,
    });
    sessions.push(session);

    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} }, { timeoutMs: 60_000 });
    assert.ok(!info.error, `server never came up -- the ghost was run, or the repair failed: ${JSON.stringify(info)}`);

    // The real assertion: the plugin repaired its OWN install rather than
    // adopting the stranger's binary.
    assert.ok(existsSync(ours), "the platform package was not reinstalled into the plugin's own data dir");
    assert.equal(
      JSON.parse(readFileSync(path.join(ours, "package.json"), "utf8")).version,
      SERVER_VERSION,
      "the repaired package must be the pinned server version, not the ghost's 0.0.3",
    );
  } finally {
    killAll(sessions);
    await sleep(3000); // let the broker exit and release the .exe inside dataDir
    rmDir(root);
    rmDir(proj);
  }
});

test("broker_idle_exits_and_releases_the_lock", async () => {
  const dataDir = mkDataDir("topodb-t4-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t4-proj-"));
  const dbPath = path.join(dataDir, "memory.redb");
  const scope = projectScopeId(proj);
  let session = null;
  let prober = null;
  try {
    // A short, test-only idle window (see broker.js's TOPODB_BROKER_IDLE_MS) --
    // NOT the production 60s. A 60-second test would be worse than no test.
    session = await connectAndInit({ dataDir, projectDir: proj, env: { TOPODB_BROKER_IDLE_MS: "1000" } });
    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} });
    assert.ok(!info.error, `db_info errored: ${JSON.stringify(info)}`);

    // Disconnect: the broker's client count drops to zero and its
    // (shortened) idle timer arms.
    await killAndWaitForExit(session);
    session = null;

    // Bounded poll for the lock's release. Prove it the same way a second
    // Claude Code window's server would notice it: open the db directly with
    // a plain topodb-mcp, no broker involved.
    const deadline = Date.now() + 8000;
    let opened = false;
    let lastErr;
    while (Date.now() < deadline && !opened) {
      prober = spawnRawServer(dbPath, scope);
      try {
        const initMsg = await prober.rpc(
          "initialize",
          { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "prober", version: "0" } },
          { timeoutMs: 2000 },
        );
        opened = !initMsg.error;
        if (!opened) lastErr = new Error(JSON.stringify(initMsg.error));
      } catch (err) {
        lastErr = err;
      } finally {
        prober.child.kill();
        prober = null;
      }
      if (!opened) await sleep(300);
    }

    assert.ok(opened, `expected the db to be openable directly after the broker's idle-exit; last error: ${lastErr?.message}`);
  } finally {
    if (session) killAll([session]);
    if (prober) killAll([prober]);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("degrades_visibly_when_memory_is_unavailable", async () => {
  const dataDir = mkDataDir("topodb-t5-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t5-proj-"));
  // Poison the db path itself so topodb-mcp -- and therefore every broker
  // that tries to spawn it -- fails to start, without touching the network
  // (node_modules is already linked in by mkDataDir, so resolveServer()
  // succeeds; it is the SERVER that can't open, not the install).
  mkdirSync(path.join(dataDir, "memory.redb"));

  let session = null;
  try {
    session = launchSession({ dataDir, projectDir: proj });

    // launch.js polls for ~5s (25 * 200ms) before giving up on ever reaching
    // a broker and falling back to degraded.js -- budget generously for that.
    const initMsg = await session.rpc(
      "initialize",
      { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "broker-test", version: "0" } },
      { timeoutMs: 15000 },
    );
    assert.ok(!initMsg.error, `expected a successful (if degraded) initialize, got: ${JSON.stringify(initMsg)}`);
    assert.match(
      initMsg.result.serverInfo.name,
      /unavailable/i,
      "a degraded server must name the cause in its serverInfo, not pretend to be healthy",
    );
    session.notify("notifications/initialized");

    const toolsMsg = await session.rpc("tools/list", {}, { timeoutMs: 3000 });
    assert.ok(!toolsMsg.error, `expected a successful (empty) tools/list, got: ${JSON.stringify(toolsMsg)}`);
    assert.deepEqual(toolsMsg.result.tools, [], "a degraded server must report no tools, not crash trying to list them");

    // The entire point of degraded.js: a broken memory backend must not take
    // the MCP server down with it.
    assert.equal(session.child.exitCode, null, "the shim must still be alive after the handshake");
    assert.equal(session.child.signalCode, null, "the shim must still be alive after the handshake");
  } finally {
    if (session) killAll([session]);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("cancelling_own_request_does_not_cancel_another_sessions_in_flight_request", async () => {
  // C2. rmcp (crates/topodb-mcp's MCP runtime) looks up notifications/cancelled's
  // params.requestId in the UPSTREAM id pool and cancels whatever it finds
  // there. The broker rewrites every REQUEST's id before forwarding it
  // upstream -- but before this fix, notifications/cancelled was forwarded
  // verbatim, carrying the CLIENT's own id straight into upstream id-space.
  // Two independent sessions routinely reuse the same small integers for
  // their own ids (most JSON-RPC clients start counting at 1), so session A
  // cancelling ITS OWN id can, by pure coincidence, equal the broker's
  // upstream id for session B's unrelated in-flight request -- silently
  // cancelling B's write. This test manufactures that exact collision on
  // purpose (a real race can't be guaranteed to reproduce it on demand) using
  // a fake upstream server (see mkFakeCancelServerDataDir) that never answers
  // a "hold" call until cancelled or flushed, so both requests are
  // deterministically still in flight when the cancel is sent.
  const dataDir = mkFakeCancelServerDataDir("topodb-t6-data-");
  const dbPath = path.join(dataDir, "memory.redb");
  const args = ["--db", dbPath, "--scope", "t6scope", "--read-scopes", "t6scope,shared"];
  const sock = socketPathFor(dbPath);

  let broker = null;
  let a = null;
  let b = null;
  try {
    broker = spawn(process.execPath, [BROKER_JS, ...args], {
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, TOPODB_BROKER_IDLE_MS: "2000" },
    });
    let brokerErr = "";
    broker.stderr.on("data", (d) => (brokerErr += d));

    await connectSocketWithRetry(sock);

    a = socketRpcClient(sock);
    b = socketRpcClient(sock);

    for (const s of [a, b]) {
      const init = await s.rpc("initialize", {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "cancel-test", version: "0" },
      }, { id: "init" });
      assert.ok(!init.error, `initialize failed: ${JSON.stringify(init)}`);
      s.notify("notifications/initialized");
    }

    // B's hold call: whatever upstream id the broker assigns it, we learn via
    // the fake server's (test-only) echo.
    const bHoldPromise = b.rpc("tools/call", { name: "hold" }, { id: "b-hold", timeoutMs: 15000 });
    const bEcho = await b.waitForNotification((n) => n.method === "notifications/test-received" && n.params.name === "hold");
    const upstreamOfB = bEcho.params.upstreamId;

    // A sends ITS OWN request using, as its own client-space id, the exact
    // number the broker just assigned to B upstream -- the natural collision
    // this bug depends on. This becomes a DIFFERENT upstream id (global
    // counter has moved on), which is exactly the point: A's client-space id
    // and A's real upstream id now legitimately differ.
    const aHoldPromise = a.rpc("tools/call", { name: "hold" }, { id: upstreamOfB, timeoutMs: 15000 });
    // A correctly-fixed broker forgets a cancelled request's pending entry the
    // moment it translates the cancel (so a stale late response from the
    // server is never misdelivered to a client that already gave up on it) --
    // so aHoldPromise is EXPECTED to never resolve. Swallow it here rather
    // than asserting on it; it gets rejected for real when `a.close()` runs
    // in `finally`.
    aHoldPromise.catch(() => {});
    const aEcho = await a.waitForNotification((n) => n.method === "notifications/test-received" && n.params.name === "hold" && n.params.upstreamId !== upstreamOfB);
    const upstreamOfA = aEcho.params.upstreamId;

    // A cancels "its own" id -- literally the id it used for the request
    // above. A correct broker must translate this to A's REAL upstream id and
    // must NEVER let the raw value (which equals B's real upstream id) reach
    // the server.
    a.notify("notifications/cancelled", { requestId: upstreamOfB });

    // Give the (mis)routed cancel time to land, then inspect the fake
    // server's OWN bookkeeping directly -- proof independent of whether any
    // response ever reaches a client, which sidesteps the "a correctly
    // cancelled request gets no further response" behavior above.
    await sleep(500);
    const counted = await b.rpc("tools/call", { name: "count_held" }, { id: "count", timeoutMs: 5000 });
    assert.ok(!counted.error, `count_held errored: ${JSON.stringify(counted)}`);
    const heldIds = counted.result.structuredContent.heldIds;
    assert.ok(
      heldIds.includes(upstreamOfB),
      `session B's in-flight request was wrongly cancelled by session A's own cancel: held=${JSON.stringify(heldIds)} expected upstreamOfB=${upstreamOfB} still present (stderr: ${brokerErr})`,
    );
    assert.ok(
      !heldIds.includes(upstreamOfA),
      `session A's own cancel should have cancelled ITS OWN request (translation), not been dropped or misrouted: held=${JSON.stringify(heldIds)} expected upstreamOfA=${upstreamOfA} to be gone`,
    );

    // End-to-end confirmation: releasing whatever is still genuinely held
    // must resolve B's original call normally, not with an error.
    const flush = await b.rpc("tools/call", { name: "flush" }, { id: "flush", timeoutMs: 15000 });
    assert.ok(!flush.error, `flush errored: ${JSON.stringify(flush)}`);
    const resB = await bHoldPromise;
    assert.ok(!resB.error, `session B's in-flight request was wrongly cancelled: ${JSON.stringify(resB)}`);
    assert.equal(resB.result.structuredContent.status, "completed");
  } finally {
    if (a) a.close();
    if (b) b.close();
    if (broker) {
      try {
        broker.kill();
      } catch {}
    }
    rmDir(dataDir);
  }
});

test("broker_stops_accepting_connections_once_idle_exit_begins", async () => {
  // C3(b). armIdleExit used to check clients.size === 0 and then call
  // server.kill() WITHOUT ever closing the listening socket -- so the broker
  // kept ACCEPTING new connections through the entire kill window (measured
  // 6/24 trials where a connection was accepted and then yanked out from
  // under the client). This test proves the fix: once the idle timer fires,
  // the socket stops accepting connections.
  //
  // IMPORTANT: this test does NOT poll by repeatedly opening probe
  // connections to the broker's socket. Every successful connection adds to
  // `clients` and calls `clearTimeout(idleTimer)` (see listen()'s connection
  // handler) -- a polling loop built that way perpetually re-arms the very
  // idle timer it is trying to observe fire, and the timer then NEVER
  // actually elapses. Instead this polls broker.log (plain file reads, no
  // socket activity) for the "idle, closing listening socket" line broker.js
  // logs at the exact point it calls srv.close() -- srv.close() stops
  // accepting synchronously, so the very next connection attempt after that
  // line appears must be refused.
  const dataDir = mkDataDir("topodb-t7-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t7-proj-"));
  const logFile = path.join(dataDir, "broker.log");
  let session = null;
  try {
    session = await connectAndInit({ dataDir, projectDir: proj, env: { TOPODB_BROKER_IDLE_MS: "600" } });
    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} });
    assert.ok(!info.error, `db_info errored: ${JSON.stringify(info)}`);

    const dbPath = path.join(dataDir, "memory.redb");
    const sock = socketPathFor(dbPath);

    // Disconnect: clients.size drops to 0 and the (shortened) idle timer arms.
    await killAndWaitForExit(session);
    session = null;

    const deadline = Date.now() + 8000;
    let sawCloseLog = false;
    while (Date.now() < deadline && !sawCloseLog) {
      try {
        if (readFileSync(logFile, "utf8").includes("idle, closing listening socket")) {
          sawCloseLog = true;
          break;
        }
      } catch {
        // log not written yet
      }
      await sleep(20);
    }
    assert.ok(sawCloseLog, "expected broker.log to record the idle-exit socket close within 8s");

    // srv.close() stops accepting synchronously (before its callback ever
    // runs), so a connection attempt made right after we observe the log line
    // must be refused -- proving accept genuinely stopped, not merely "the
    // whole process eventually vanished so of course connects fail."
    const accepted = await new Promise((res) => {
      const c = net.connect(sock);
      c.on("connect", () => {
        c.destroy();
        res(true);
      });
      c.on("error", () => res(false));
    });
    assert.equal(accepted, false, "expected the broker to refuse a connection made right after idle-exit's socket close was logged");
  } finally {
    if (session) killAll([session]);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("shim_degrades_instead_of_exiting_cleanly_when_broker_dies_mid_session", async () => {
  // C3(a). launch.js's relay() used to do `conn.on("close", () => process.exit(0))`.
  // Killing the broker mid-session made the shim exit CODE 0 -- which Claude
  // Code reads as a clean, intentional shutdown: no degraded server, no
  // explanation, while the skill keeps telling the agent to call
  // search_memories against a server that no longer exists. This test kills
  // the REAL broker process out from under a live, already-initialized
  // session and checks the shim survives and starts answering with an
  // explanatory error instead of vanishing.
  const dataDir = mkDataDir("topodb-t8-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t8-proj-"));
  const logFile = path.join(dataDir, "broker.log");
  let session = null;
  try {
    session = await connectAndInit({ dataDir, projectDir: proj });
    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} });
    assert.ok(!info.error, `db_info errored: ${JSON.stringify(info)}`);

    // Find the REAL broker's pid from its own log line (see broker.js's
    // "listening on ... (pid=...)") -- distinct from `session.child`, which is
    // launch.js, a thin client of the broker, not the broker itself.
    const logText = readFileSync(logFile, "utf8");
    const m = logText.match(/listening on .*\(pid=(\d+)\)/);
    assert.ok(m, `could not find the broker's pid in broker.log:\n${logText}`);
    const brokerPid = Number(m[1]);

    process.kill(brokerPid);

    // Give the shim a moment to notice its socket closed, then keep using the
    // SAME session -- the exact scenario a live Claude Code window is in.
    await sleep(500);
    const afterDeath = await session.rpc("tools/call", { name: "db_info", arguments: {} }, { timeoutMs: 5000 });

    // THE assertion: the shim must still be running, not silently exited.
    assert.equal(session.child.exitCode, null, "the shim must not have exited after the broker died mid-session");
    assert.equal(session.child.signalCode, null, "the shim must not have exited after the broker died mid-session");
    // And it must say WHY memory is gone, not pretend the call succeeded.
    assert.ok(afterDeath.error, `expected an explanatory error after the broker died, got: ${JSON.stringify(afterDeath)}`);
    assert.match(afterDeath.error.message, /broker/i, "the error should explain the broker is what died");
  } finally {
    if (session) killAll([session]);
    rmDir(dataDir);
    rmDir(proj);
  }
});
