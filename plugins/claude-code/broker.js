#!/usr/bin/env node
// The detached broker. ONE of these owns the database; every Claude Code
// session's launch.js is a client of it.
//
// Why a detached process and not a "first session is the leader" scheme: when a
// leader's window closes, its process dies and every other session loses memory
// mid-conversation, forcing re-election and in-flight request replay. A
// session-independent broker deletes that problem — there is no leader, so
// there is no leader death.
//
// See specs/2026-07-12-plugin-broker-design.md.
import { spawn } from "node:child_process";
import net from "node:net";
import { createRequire } from "node:module";
import { appendFileSync, unlinkSync } from "node:fs";
import path from "node:path";
import { socketPathFor, lineReader } from "./ipc.js";

// Overridable so tests can prove the idle-exit/lock-release behavior without
// waiting 60 real seconds for it.
const IDLE_EXIT_MS = Number(process.env.TOPODB_BROKER_IDLE_MS) || 60_000;

const args = process.argv.slice(2);
const dbPath = args[args.indexOf("--db") + 1];
const sock = socketPathFor(dbPath);
const logFile = path.join(path.dirname(dbPath), "broker.log");

// The broker is spawned detached with stdio ignored, so nothing it prints is
// ever seen. Without a log, a broker that fails to start is undiagnosable.
const log = (m) => {
  try {
    appendFileSync(logFile, `[${new Date().toISOString()}] ${m}\n`);
  } catch {}
};

const require = createRequire(import.meta.url);
const serverBin = require.resolve("@topodb/topodb-mcp/bin/topodb-mcp.js", {
  paths: [path.join(path.dirname(dbPath), "node_modules")],
});

const server = spawn(process.execPath, [serverBin, ...args], {
  stdio: ["pipe", "pipe", "pipe"],
});

// lineReader accumulates raw chunks into a string buffer (`buf += chunk`). If
// stdout stayed in binary mode, a multi-byte UTF-8 character split across two
// chunk boundaries would be decoded independently on each half and corrupt
// the JSON. Force utf8 decoding at the stream level, before either handler
// below ever sees a chunk.
server.stdout.setEncoding("utf8");

let serverErr = "";
server.stderr.on("data", (d) => (serverErr += d));

// THE ELECTION. Two shims racing both spawn a broker; both land here; exactly
// one wins redb's exclusive lock. The loser's server exits with
// DatabaseAlreadyOpen — so it exits too, WITHOUT ever binding the socket. The
// winner binds it and both shims connect to the winner. redb's lock IS the
// election; we do not implement one.
//
// `bound` tracks whether THIS process actually bound `sock`. `sock` is a pure
// function of the db path -- the SAME path for every broker racing on this
// db -- so a loser (which never binds) must NEVER unlink it: the loser can
// exit AFTER the winner has already bound, and an unconditional unlink would
// delete the WINNER's live socket file out from under it, reintroducing the
// exact "only the first window gets memory" bug this broker exists to fix.
let bound = false;
server.on("exit", (code) => {
  log(`server exited code=${code}: ${serverErr.trim()}`);
  try {
    if (bound && process.platform !== "win32") unlinkSync(sock);
  } catch {}
  process.exit(code ?? 1);
});

// --- multiplexing state ---
const clients = new Set();
let nextUpstreamId = 1;
const pending = new Map(); // upstreamId -> { client, originalId }
let initResult = null; // cached `initialize` result, replayed to every client
let idleTimer = null;

const send = (sockConn, obj) => {
  try {
    sockConn.write(JSON.stringify(obj) + "\n");
  } catch {}
};
const toServer = (obj) => server.stdin.write(JSON.stringify(obj) + "\n");

// --- server -> clients ---
server.stdout.on(
  "data",
  lineReader((line) => {
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg.id !== undefined && pending.has(msg.id)) {
      // Rewrite the id back to what THIS client used. Without this, two clients
      // that both sent `id: 1` would receive each other's answers — silent,
      // plausible, and catastrophic for a memory tool.
      const { client, originalId } = pending.get(msg.id);
      pending.delete(msg.id);
      send(client, { ...msg, id: originalId });
    } else if (msg.id === undefined) {
      for (const c of clients) send(c, msg); // server-initiated notification
    }
  }),
);

// --- the one upstream handshake, whose result every client gets ---
const INIT_ID = 0;
// 15s: generous for a cold server start, but bounded. Without this timeout, an
// `initialize` ERROR response, or a server that starts but never answers at
// all, leaves initResult null and listen() never called -- the broker just
// sits there forever, holding redb's exclusive lock, and memory is dead
// machine-wide until someone finds and kills it by hand. Every other failure
// path in this file self-heals (the client polls, gives up, and degrades);
// this is the one that doesn't, so it gets its own explicit timeout.
const HANDSHAKE_TIMEOUT_MS = 15_000;
const handshakeTimer = setTimeout(() => {
  log(`initialize handshake timed out after ${HANDSHAKE_TIMEOUT_MS}ms; killing server`);
  server.stdout.off("data", onInit);
  server.kill();
}, HANDSHAKE_TIMEOUT_MS);

toServer({
  jsonrpc: "2.0",
  id: INIT_ID,
  method: "initialize",
  params: {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "topodb-broker", version: "1" },
  },
});

const onInit = lineReader((line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.id !== INIT_ID) return;
  if (msg.error) {
    // A response arrived, so the server isn't wedged -- but it refused to
    // initialize. Don't wait out the rest of the timeout for a reply that
    // will never come; fail now.
    clearTimeout(handshakeTimer);
    log(`initialize failed: ${JSON.stringify(msg.error)}; killing server`);
    server.stdout.off("data", onInit);
    server.kill();
    return;
  }
  if (msg.result) {
    clearTimeout(handshakeTimer);
    initResult = msg.result;
    toServer({ jsonrpc: "2.0", method: "notifications/initialized" });
    server.stdout.off("data", onInit);
    listen(); // only NOW do we bind the socket: we hold the db and are ready
  }
});
server.stdout.on("data", onInit);

// The net.Server, reachable from armIdleExit so idle-exit can stop accepting
// connections before deciding to kill the server -- see armIdleExit.
let srv = null;

function armIdleExit() {
  clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    if (clients.size > 0) return;
    // Stop ACCEPTING first, then re-check. The bug this closes: checking
    // clients.size and then calling server.kill() without ever closing the
    // listening socket leaves it accepting connections through the entire
    // kill window -- measured 6/24 trials where a connection was accepted and
    // then yanked out from under a client that believed it had a working
    // broker. srv.close() stops new accepts synchronously; only once that is
    // true do we re-check whether someone slipped in before it took effect.
    log("idle, closing listening socket");
    srv.close(() => {
      if (clients.size > 0) {
        // Someone connected between the check above and close() taking
        // effect. Reopen and let this connection keep the broker alive.
        srv.listen(sock);
        return;
      }
      log("idle, exiting");
      server.kill();
    });
  }, IDLE_EXIT_MS);
}

function listen() {
  srv = net.createServer((conn) => {
    clients.add(conn);
    clearTimeout(idleTimer);

    conn.on(
      "data",
      lineReader((line) => {
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          return;
        }

        // Each session is its own MCP client and sends its own initialize.
        // Forwarding N of them to one server is wrong; answer from cache.
        if (msg.method === "initialize") {
          send(conn, { jsonrpc: "2.0", id: msg.id, result: initResult });
          return;
        }
        if (msg.method === "notifications/initialized") return; // per-client, no server meaning

        if (msg.method === "notifications/cancelled") {
          // MUST be handled before the generic `msg.id === undefined` forward
          // below, and MUST NEVER forward a client-space id upstream. Claude
          // Code sends this when a user hits Esc, carrying the CLIENT's own
          // id in params.requestId -- but pending's keys are the broker's
          // rewritten UPSTREAM ids. If we forwarded params.requestId
          // untranslated, session A cancelling its own client-side id 7 would
          // tell the server to cancel whatever UPSTREAM id happens to be 7 --
          // very possibly session B's in-flight create_memory, whose write
          // then silently never happens while B's call hangs. Translate
          // through `pending`, scoped to THIS connection; if no match, the id
          // is unknown or already resolved, so drop it rather than guess.
          const rid = msg.params?.requestId;
          for (const [up, p] of pending) {
            if (p.client === conn && p.originalId === rid) {
              pending.delete(up);
              toServer({ ...msg, params: { ...msg.params, requestId: up } });
              return;
            }
          }
          return; // unknown/foreign id: DROP. Never forward an untranslated id.
        }

        if (msg.id === undefined) {
          toServer(msg); // client notification: forward as-is
          return;
        }
        const up = nextUpstreamId++;
        pending.set(up, { client: conn, originalId: msg.id });
        toServer({ ...msg, id: up });
      }),
    );

    const drop = () => {
      clients.delete(conn);
      for (const [up, p] of pending) if (p.client === conn) pending.delete(up);
      if (clients.size === 0) armIdleExit();
    };
    conn.on("close", drop);
    conn.on("error", drop);
  });

  srv.on("error", (e) => {
    // A stale unix socket file from a broker that died without cleanup. We only
    // get here after a connect attempt already failed, so nothing is listening.
    if (e.code === "EADDRINUSE" && process.platform !== "win32") {
      try {
        unlinkSync(sock);
        srv.listen(sock);
        return;
      } catch {}
    }
    log(`listen failed: ${e.message}`);
    process.exit(1);
  });

  srv.listen(sock, () => {
    bound = true;
    // pid included so a human (or a test) reading broker.log can find and, if
    // truly necessary, kill a wedged broker by hand -- see I4's motivation.
    log(`listening on ${sock} (pid=${process.pid})`);
    armIdleExit();
  });
}
