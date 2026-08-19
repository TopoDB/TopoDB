#!/usr/bin/env node
// PostToolUse (Read|Bash|Edit|Write|MultiEdit|Grep|Glob|WebFetch): land the raw
// artifact into the warehouse spool. Subagents ARE captured (attributed to the session).
import { readStdin, parseJson } from "../core/hook-io.js";
import { captureArtifact } from "../core/hooks/capture.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw); if (!p) return;
  const toolName = String(p.tool_name ?? "");
  if (toolName.includes("__")) return; // MCP tools are not artifacts
  captureArtifact({ dataDir: process.env.CLAUDE_PLUGIN_DATA, env: process.env, projectDir: process.env.CLAUDE_PROJECT_DIR ?? p.cwd,
    sessionId: p.session_id, toolName, toolInput: p.tool_input, toolResponse: p.tool_response ?? p.tool_output, cwd: p.cwd, agent: p.agent_id, harness: "claude-code" });
}
main().catch(() => {}).finally(() => process.exit(0));
