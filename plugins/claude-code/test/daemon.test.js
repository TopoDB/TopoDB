// Cross-process concurrency tests for the TopoDB daemon (`topodb-mcp --socket`).
//
// launch.js is a thin stdio<->socket client now: it connects to the resident
// Rust daemon (spawning `topodb-mcp --socket` if none is up) which owns the one
// memory.redb and multiplexes every session's connection itself. These tests
// therefore exercise the daemon THROUGH launch.js -- election, scope stamping,
// idle-exit, degraded fallback -- exactly as a real Claude Code session does.
// The broker.js multiplexing machinery these tests once covered (JSON-RPC id
// rewriting, cancellation-id translation, listener-close scraping) is retired
// with the code; those id/cancel concerns no longer exist because each daemon
// connection is its own rmcp session.
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
import { lineReader, socketPathFor, helloFrame } from "../core/ipc.js";
import { projectScopeId } from "../core/scope-id.js";
import { rmWithGraceSync } from "./fsgrace.js";
import { SERVER_VERSION } from "../core/server-args.js";

const require = createRequire(import.meta.url);
// The real launcher's own platform-key logic, not a copy of it. A second copy of
// this table is exactly the kind of drift that produced the bug below.
const { binaryFileName } = require("@topodb/topodb-mcp/bin/topodb-mcp.js");
const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.join(HERE, "..");

// The pinned npm @topodb/topodb-mcp (SERVER_VERSION) predates `--socket`, so
// every end-to-end session in this file needs a daemon-capable binary that only
// a local `cargo build` provides. TOPODB_MCP_SERVER_BIN is launch.js's seam for
// exactly this; setting it here reaches every spawned shim because the test
// helpers spread process.env into child env. Without the binary, skip rather
// than fail — same gate the pi recorder tests use.
const REPO_ROOT = path.join(PLUGIN_ROOT, "..", "..");
const DAEMON_BIN = path.join(
  REPO_ROOT,
  "target",
  "debug",
  process.platform === "win32" ? "topodb-mcp.exe" : "topodb-mcp",
);
const HAVE_DAEMON_BIN = existsSync(DAEMON_BIN);
if (HAVE_DAEMON_BIN) process.env.TOPODB_MCP_SERVER_BIN = DAEMON_BIN;
const needsDaemonBin = HAVE_DAEMON_BIN
  ? {}
  : { skip: "needs target/debug/topodb-mcp (cargo build): pinned npm server predates --socket" };
// 0.0.17 is the last release WITHOUT --socket. The two npm-resolution tests run
// the real pinned binary end to end, so they cannot pass until the pin advances
// to a daemon-capable release — at which point this skip lifts by itself.
const pinnedPredatesDaemon =
  SERVER_VERSION === "0.0.17"
    ? { skip: "pinned @topodb/topodb-mcp predates --socket; unskips when the pin advances" }
    : {};
const LAUNCH_JS = path.join(PLUGIN_ROOT, "launch.js");

// Windows CI runners are slow enough that daemon handshakes have blown the
// 8s deadline twice on unrelated PRs (#56's flake family) — triple it there.
const DEFAULT_RPC_TIMEOUT_MS = process.platform === "win32" ? 24000 : 8000;

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
  // directory while the just-killed daemon still holds a handle on
  // memory.redb / its log — rmdir then throws ENOTEMPTY
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
    // Keep every daemon THIS suite spawns short-lived. Without this it
    // inherits the production 60s idle timeout and lingers as an orphan
    // process for up to a minute after each test finishes.
    TOPODB_DAEMON_IDLE_MS: "5000",
    ...env,
  });
}

async function connectAndInit({ dataDir, projectDir, env, initTimeoutMs }) {
  const session = launchSession({ dataDir, projectDir, env });
  const initMsg = await session.rpc(
    "initialize",
    { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "daemon-test", version: "0" } },
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

/** Spawn the resident daemon DIRECTLY (`topodb-mcp --socket`), bypassing
 * launch.js, so a test owns its process handle. Needed to kill the daemon out
 * from under a live session without scraping its pid from a log file -- the
 * broker-era trick, retired with broker.log. Returns the child; wait for its
 * socket with connectSocketWithRetry before pointing a shim at it. */
function spawnDaemon(dbPath, scope, env = {}) {
  // Spawn the LOCAL cargo build directly — the pinned npm shim's binary
  // predates `--socket` (see the DAEMON_BIN seam at the top of this file), so
  // resolving through @topodb/topodb-mcp would launch a server that dies on
  // the flag and the test would time out waiting for a socket.
  const sock = socketPathFor(dbPath);
  return spawn(
    DAEMON_BIN,
    ["--socket", sock, "--db", dbPath, "--scope", scope, "--read-scopes", `${scope},shared`],
    { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, ...env } },
  );
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

/** A raw client on the daemon's socket, standing in for a session's shim.
 * Connects DIRECTLY to the daemon's socket, bypassing launch.js entirely, so a
 * test can act as its own MCP session -- the reference client broker-client.js
 * (hooks) mirrors.
 *
 * It MUST open with a hello frame, because that is what a real shim does
 * (`launch.js`'s `relay`) and the daemon refuses to serve a request from a
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

test("two_concurrent_sessions_both_get_memory", needsDaemonBin, async () => {
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

test("answers_do_not_cross_between_sessions", needsDaemonBin, async () => {
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
    // at the same instant. Each shim is its OWN connection = its own rmcp
    // session on the daemon, so their id-spaces are independent by
    // construction; this pins that isolation end to end -- a crossed answer
    // (B receiving A's) would be silent, plausible, and catastrophic for a
    // memory tool.
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

test("startup_race_elects_one_daemon", needsDaemonBin, async () => {
  const dataDir = mkDataDir("topodb-t3-data-");
  const projDirs = Array.from({ length: 5 }, (_, i) => mkdtempSync(path.join(tmpdir(), `topodb-t3-proj${i}-`)));
  const sessions = [];
  try {
    // Five shims race a COLD db at once. Each spawns `topodb-mcp --socket`, but
    // only one wins redb's exclusive lock and binds the socket; the other four
    // daemons exit with DatabaseAlreadyOpen without ever binding it (redb's lock
    // IS the election -- see the daemon design doc), and their shims connect to
    // the winner instead.
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

test("each_session_writes_to_its_own_project_scope", needsDaemonBin, async () => {
  const dataDir = mkDataDir("topodb-t3b-data-");
  const projA = mkdtempSync(path.join(tmpdir(), "topodb-t3b-projA-"));
  const projB = mkdtempSync(path.join(tmpdir(), "topodb-t3b-projB-"));
  const sessions = [];
  try {
    // THE SCOPE-BLEED REGRESSION TEST. The tests above already prove two
    // projects SHARE one database and one daemon -- which is the whole point.
    // What none of them assert is which SCOPE each session lands in, and that
    // omission is exactly why the bug shipped: `serverArgs()` bakes
    // `--scope <ULID(projectDir)>` into argv, ONE daemon wins the lock with
    // whichever session's argv got there first, and `socketPathFor(dbPath)`
    // keys the daemon on the DB PATH ALONE -- which is identical for every
    // project. If the daemon ran every connection under its own startup scope
    // instead of the per-connection hello scope, session B would silently
    // inherit A's scope. The hello frame each shim sends is what prevents that.
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

test("one_project_cannot_read_another_projects_memory", needsDaemonBin, async () => {
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

test("repairs an install whose platform binary is missing", pinnedPredatesDaemon, async () => {
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
    // the topodb-mcp.exe the daemon is running lives INSIDE the directory the
    // teardown has to delete, and Windows will not delete a running executable.
    // The daemon has to idle-exit before cleanup can succeed.
    const session = await connectAndInit({
      dataDir,
      projectDir: proj,
      // Clear the file-level TOPODB_MCP_SERVER_BIN (set when a local cargo
      // build exists): these two tests exercise REAL npm resolution/repair, so
      // the spawned launch.js must NOT be handed a native-binary override.
      // Empty string reads as unset in launch.js.
      //
      // A short daemon idle window so the daemon reaps FAST after launch.js is
      // killed and releases the data dir before teardown deletes it — on Windows
      // a still-running topodb-mcp.exe can't be unlinked (EPERM).
      env: { TOPODB_DAEMON_IDLE_MS: "500", TOPODB_MCP_SERVER_BIN: "" },
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
    // Wait out the (shortened) idle window so the daemon exits and releases the
    // running topodb-mcp.exe that lives inside dataDir — see above.
    await sleep(3000);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("never runs a stale binary resolved from outside its own data dir", pinnedPredatesDaemon, async () => {
  // The bug AS IT ACTUALLY HAPPENED, end to end. Missing platform package in the
  // data dir + a stale copy of that package in an ANCESTOR node_modules.
  // `require.resolve` walks up, finds the stale one, and returns it. Resolution
  // SUCCEEDS -- so the shim's "not installed" error never fires, no repair is
  // attempted, and the daemon executes a topodb-mcp@0.0.3 while SERVER_VERSION,
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
      // Clear the file-level TOPODB_MCP_SERVER_BIN (set when a local cargo
      // build exists): these two tests exercise REAL npm resolution/repair, so
      // the spawned launch.js must NOT be handed a native-binary override.
      // Empty string reads as unset in launch.js.
      //
      // A short daemon idle window so the daemon reaps FAST after launch.js is
      // killed and releases the data dir before teardown deletes it — on Windows
      // a still-running topodb-mcp.exe can't be unlinked (EPERM).
      env: { TOPODB_DAEMON_IDLE_MS: "500", TOPODB_MCP_SERVER_BIN: "" },
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
    await sleep(3000); // let the daemon exit and release the .exe inside dataDir
    rmDir(root);
    rmDir(proj);
  }
});

test("daemon_idle_exits_and_releases_the_lock", needsDaemonBin, async () => {
  const dataDir = mkDataDir("topodb-t4-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t4-proj-"));
  const dbPath = path.join(dataDir, "memory.redb");
  const scope = projectScopeId(proj);
  let session = null;
  let prober = null;
  try {
    // A short, test-only idle window (the daemon's TOPODB_DAEMON_IDLE_MS) --
    // NOT the production 60s. A 60-second test would be worse than no test.
    session = await connectAndInit({ dataDir, projectDir: proj, env: { TOPODB_DAEMON_IDLE_MS: "1000" } });
    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} });
    assert.ok(!info.error, `db_info errored: ${JSON.stringify(info)}`);

    // Disconnect: the daemon's connection count drops to zero and its
    // (shortened) idle timer arms.
    await killAndWaitForExit(session);
    session = null;

    // Bounded poll for the lock's release. Prove it the same way a second
    // Claude Code window's server would notice it: open the db directly with
    // a plain topodb-mcp, no daemon involved.
    const deadline = Date.now() + DEFAULT_RPC_TIMEOUT_MS;
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

    assert.ok(opened, `expected the db to be openable directly after the daemon's idle-exit; last error: ${lastErr?.message}`);
  } finally {
    if (session) killAll([session]);
    if (prober) killAll([prober]);
    rmDir(dataDir);
    rmDir(proj);
  }
});

test("degrades_visibly_when_memory_is_unavailable", needsDaemonBin, async () => {
  const dataDir = mkDataDir("topodb-t5-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t5-proj-"));
  // Poison the db path itself so topodb-mcp -- and therefore any daemon that
  // tries to open it -- fails to start, without touching the network
  // (node_modules is already linked in by mkDataDir, so resolveServer()
  // succeeds; it is the SERVER that can't open, not the install).
  mkdirSync(path.join(dataDir, "memory.redb"));

  let session = null;
  try {
    session = launchSession({ dataDir, projectDir: proj });

    // launch.js polls for ~10s (50 * 200ms) before giving up on ever reaching
    // a daemon and falling back to degraded.js -- budget generously for that.
    const initMsg = await session.rpc(
      "initialize",
      { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "daemon-test", version: "0" } },
      { timeoutMs: 20000 },
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

test("shim_degrades_instead_of_exiting_cleanly_when_daemon_dies_mid_session", needsDaemonBin, async () => {
  // C3(a). launch.js's relay() must NOT do `conn.on("close", () => process.exit(0))`:
  // killing the daemon mid-session that way makes the shim exit CODE 0 -- which
  // Claude Code reads as a clean, intentional shutdown: no degraded server, no
  // explanation, while the skill keeps telling the agent to call
  // search_memories against a server that no longer exists. This test kills the
  // REAL daemon out from under a live, already-initialized session and checks
  // the shim survives and starts answering with an explanatory error instead of
  // vanishing.
  //
  // The daemon is spawned DIRECTLY here (not via launch.js) so this test owns
  // its process handle and can kill it precisely -- the broker-era trick of
  // scraping a pid out of broker.log is retired with the log. launch.js then
  // finds that socket already up and connects to it instead of spawning its own.
  const dataDir = mkDataDir("topodb-t8-data-");
  const proj = mkdtempSync(path.join(tmpdir(), "topodb-t8-proj-"));
  const dbPath = path.join(dataDir, "memory.redb");
  const scope = projectScopeId(proj);
  const sock = socketPathFor(dbPath);
  let daemon = null;
  let session = null;
  try {
    // Idle-exit disabled (0 = never): this daemon must stay up until WE kill
    // it, so an idle-exit can never masquerade as the mid-session death.
    daemon = spawnDaemon(dbPath, scope, { TOPODB_DAEMON_IDLE_MS: "0" });
    await connectSocketWithRetry(sock);

    session = await connectAndInit({ dataDir, projectDir: proj });
    const info = await session.rpc("tools/call", { name: "db_info", arguments: {} });
    assert.ok(!info.error, `db_info errored: ${JSON.stringify(info)}`);

    // Kill the daemon out from under the live session: the shim's socket
    // closes and relay() must fall back to a degraded server, NOT exit 0.
    await new Promise((resolve) => {
      daemon.once("exit", () => resolve());
      daemon.kill();
    });
    daemon = null;

    // Give the shim a moment to notice its socket closed, then keep using the
    // SAME session -- the exact scenario a live Claude Code window is in.
    await sleep(500);
    const afterDeath = await session.rpc("tools/call", { name: "db_info", arguments: {} }, { timeoutMs: 5000 });

    // THE assertion: the shim must still be running, not silently exited.
    assert.equal(session.child.exitCode, null, "the shim must not have exited after the daemon died mid-session");
    assert.equal(session.child.signalCode, null, "the shim must not have exited after the daemon died mid-session");
    // And it must say WHY memory is gone, not pretend the call succeeded.
    assert.ok(afterDeath.error, `expected an explanatory error after the daemon died, got: ${JSON.stringify(afterDeath)}`);
    assert.match(afterDeath.error.message, /daemon/i, "the error should explain the daemon is what died");
  } finally {
    if (session) killAll([session]);
    if (daemon) {
      try {
        daemon.kill();
      } catch {}
    }
    rmDir(dataDir);
    rmDir(proj);
  }
});
