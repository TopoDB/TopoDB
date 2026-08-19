#!/usr/bin/env node
// PostToolUse (Read|Bash|Edit|Write|MultiEdit|Grep|Glob|WebFetch): land the
// raw artifact into the warehouse spool. One appendFileSync, no broker, no
// stdout, exit 0 always. Subagents ARE captured (attributed to the session).
import { sessionScopes } from "../core/server-args.js";
import { warehouseDisabled, artifactEvent, appendSpool } from "../core/warehouse-spool.js";

async function main() {
  const raw = await new Promise((r) => { let buf = ""; process.stdin.on("data", (d) => (buf += d)); process.stdin.on("end", () => r(buf)); });
  if (warehouseDisabled(process.env)) return;
  let p; try { p = JSON.parse(raw); } catch { return; }
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir || !p.session_id) return;
  const toolName = String(p.tool_name ?? "");
  if (toolName.includes("__")) return; // MCP tools are not artifacts
  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? p.cwd;
  if (!projectDir) return;
  const { scope } = sessionScopes({ projectDir });
  const ev = artifactEvent({ toolName, toolInput: p.tool_input, toolResponse: p.tool_response ?? p.tool_output,
    sessionId: p.session_id, scope, cwd: p.cwd, agent: p.agent_id });
  if (!ev) return;
  appendSpool(dataDir, p.session_id, ev, process.env);
}
main().catch(() => {}).finally(() => process.exit(0));
