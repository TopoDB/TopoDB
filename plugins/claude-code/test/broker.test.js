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
import { mkdtempSync, mkdirSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import path from "node:path";
import { lineReader } from "../ipc.js";
import { projectScopeId } from "../scope-id.js";

const require = createRequire(import.meta.url);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.join(HERE, "..");
const LAUNCH_JS = path.join(PLUGIN_ROOT, "launch.js");

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

function rmDir(dir) {
  // Windows kill() is asynchronous (TerminateProcess); the OS can still be
  // releasing a file handle on memory.redb when this runs, racing an
  // EBUSY/EPERM. maxRetries + retryDelay give the handle time to let go.
  rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
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
    const [a, b] = await Promise.all([
      connectAndInit({ dataDir, projectDir: projA }),
      connectAndInit({ dataDir, projectDir: projB }),
    ]);
    sessions.push(a, b);

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
    const [a, b] = await Promise.all([
      connectAndInit({ dataDir, projectDir: projA }),
      connectAndInit({ dataDir, projectDir: projB }),
    ]);
    sessions.push(a, b);

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
