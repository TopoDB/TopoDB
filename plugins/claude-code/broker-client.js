// Hook-side broker client: a short-lived MCP session over the broker's
// socket. Hooks are ephemeral processes; each connects, speaks a few
// calls, and exits. NEVER spawns the broker (that is the MCP shim's job
// — see the design spec's amendment): no broker means the caller
// degrades, it does not bootstrap.
import net from "node:net";
import { socketPathFor, helloFrame, lineReader } from "./ipc.js";
import { serverArgs, sessionScopes } from "./server-args.js";

export async function connectForProject({ projectDir, dataDir, connectTimeoutMs = 1500 }) {
  const args = serverArgs({ projectDir, dataDir });
  const dbPath = args[args.indexOf("--db") + 1];
  const sock = socketPathFor(dbPath);
  const conn = await new Promise((resolve) => {
    const c = net.connect(sock);
    const t = setTimeout(() => {
      c.destroy();
      resolve(null);
    }, connectTimeoutMs);
    c.once("connect", () => {
      clearTimeout(t);
      resolve(c);
    });
    c.once("error", () => {
      clearTimeout(t);
      resolve(null);
    });
  });
  if (!conn) return null;

  // Must be first on the wire, before any JSON-RPC — the broker refuses to
  // forward a request from a connection whose scope it does not know (see
  // test/broker.test.js's socketRpcClient for the working reference).
  conn.write(helloFrame(sessionScopes({ projectDir })));

  let nextId = 1;
  const pending = new Map();
  conn.on(
    "data",
    lineReader((line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return; // not ours to diagnose — hooks must never crash on noise
      }
      const p = msg && pending.get(msg.id);
      if (!p) return; // notification or unknown id
      pending.delete(msg.id);
      if (msg.error) p.reject(new Error(`rpc error: ${msg.error.message ?? JSON.stringify(msg.error)}`));
      else p.resolve(msg.result);
    }),
  );
  conn.on("error", () => {
    for (const p of pending.values()) p.reject(new Error("broker socket error"));
    pending.clear();
  });
  conn.on("close", () => {
    for (const p of pending.values()) p.reject(new Error("broker socket closed"));
    pending.clear();
  });

  const rpc = (method, params, timeoutMs) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      const t = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`rpc "${method}" timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      pending.set(id, {
        resolve: (v) => {
          clearTimeout(t);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(t);
          reject(e);
        },
      });
      conn.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });

  // MCP handshake: the broker treats each connection as a session. Params
  // mirror test/broker.test.js's socketRpcClient exactly (protocolVersion,
  // empty capabilities, clientInfo) — that raw-socket client is the working
  // reference for what this broker accepts per-connection.
  await rpc(
    "initialize",
    {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "topodb-hook", version: "0.1.0" },
    },
    2000,
  );
  conn.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");

  return {
    async call(name, args, timeoutMs = 2000) {
      const res = await rpc("tools/call", { name, arguments: args }, timeoutMs);
      if (res && res.isError) {
        const text = res.content?.[0]?.text ?? "tool error";
        throw new Error(`${name}: ${text}`);
      }
      if (res && res.structuredContent !== undefined) return res.structuredContent;
      const text = res?.content?.[0]?.text;
      try {
        return text !== undefined ? JSON.parse(text) : {};
      } catch {
        return {};
      }
    },
    close() {
      conn.destroy();
    },
  };
}
