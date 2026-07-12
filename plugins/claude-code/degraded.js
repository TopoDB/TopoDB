// A VALID MCP server that reports why memory is unavailable.
//
// This exists because a failed MCP server is nearly invisible in Claude Code —
// `/mcp` shows "failed" and nothing else does — while the SKILL still loads and
// still tells the agent to call search_memories. A server that comes up and
// explains that it is useless is enormously better than one that vanishes.
import { lineReader } from "./ipc.js";

export function serveDegraded(reason) {
  const out = (o) => process.stdout.write(JSON.stringify(o) + "\n");
  process.stdin.on(
    "data",
    lineReader((line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      if (msg.method === "initialize") {
        out({
          jsonrpc: "2.0",
          id: msg.id,
          result: {
            protocolVersion: "2024-11-05",
            capabilities: { tools: {} },
            serverInfo: { name: `topodb (unavailable: ${reason})`, version: "0" },
          },
        });
      } else if (msg.method === "tools/list") {
        out({ jsonrpc: "2.0", id: msg.id, result: { tools: [] } });
      } else if (msg.id !== undefined) {
        out({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32603, message: `topodb memory is unavailable: ${reason}` },
        });
      }
    }),
  );
}
