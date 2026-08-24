#!/usr/bin/env node
// PostToolUse (all tools, async): land the raw artifact into the warehouse
// spool. Codex normalizes apply_patch into Edit/Write payloads, so the Claude
// tool vocabulary is already the wire shape — no name mapping needed. topodb's
// own MCP tools feed episode state instead (retrievals; remember marks the
// session captured); a foreign server's same-named tool is never mistaken for
// ours. No daemon, nothing on stdout, exit 0 always.
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { warehouseDisabled } from "../core/warehouse-spool.js";
import { captureArtifact } from "../core/hooks/capture.js";
import { isTopodbTool, bareToolName, recordRetrieval, recordMemoryWrite, WRITE_TOOLS } from "../core/hooks/retrieval.js";
import { hookContext, HARNESS } from "./_env.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "PostToolUse", raw });
  if (!p) return;
  if (warehouseDisabled(process.env)) return; // TOPODB_WAREHOUSE=0 / TOPODB_RECORDING=0 suppress everything
  const toolName = String(p.tool_name ?? "");
  const toolResult = p.tool_response ?? p.tool_output;
  if (/__|\/|:/.test(toolName)) {
    if (!isTopodbTool(toolName)) return; // qualified but not ours: no state
    recordRetrieval({ dataDir, sessionId, toolName, toolInput: p.tool_input, toolResult });
    if (WRITE_TOOLS.includes(bareToolName(toolName))) {
      recordMemoryWrite({ dataDir, env: process.env, projectDir, sessionId, toolInput: p.tool_input, toolResult, harness: HARNESS });
    }
    return;
  }
  captureArtifact({ dataDir, env: process.env, projectDir, sessionId, toolName, toolInput: p.tool_input, toolResponse: toolResult, cwd: p.cwd, agent: p.agent_id, harness: HARNESS });
}
runHook(main, { deadlineMs: 4000 });
