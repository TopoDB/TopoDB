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

const IDLE_EXIT_MS = 60_000;

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
server.on("exit", (code) => {
  log(`server exited code=${code}: ${serverErr.trim()}`);
  try {
    if (process.platform !== "win32") unlinkSync(sock);
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
  if (msg.id === INIT_ID && msg.result) {
    initResult = msg.result;
    toServer({ jsonrpc: "2.0", method: "notifications/initialized" });
    server.stdout.off("data", onInit);
    listen(); // only NOW do we bind the socket: we hold the db and are ready
  }
});
server.stdout.on("data", onInit);

function armIdleExit() {
  clearTimeout(idleTimer);
  idleTimer = setTimeout(() => {
    if (clients.size === 0) {
      log("idle, exiting");
      server.kill();
    }
  }, IDLE_EXIT_MS);
}

function listen() {
  const srv = net.createServer((conn) => {
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
    log(`listening on ${sock}`);
    armIdleExit();
  });
}
