#!/usr/bin/env node
// PostToolUse (matched to topodb retrieval tools): append what the model just
// retrieved to this session's episode state file. No daemon contact, no stdout.
import { readStdin, parseJson, debugDump, recordingDisabled } from "../core/hook-io.js";
import { recordRetrieval } from "../core/hooks/retrieval.js";

async function main() {
  const raw = await readStdin();
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  debugDump({ dataDir, env: process.env, eventName: "last-payload", raw }); // keeps the historical debug-last-payload.json name
  if (recordingDisabled(process.env)) return;
  const p = parseJson(raw); if (!p) return;
  if (p.agent_id || p.agent_type) return; // main sessions only
  recordRetrieval({ dataDir, sessionId: p.session_id, toolName: p.tool_name, toolInput: p.tool_input, toolResult: p.tool_output ?? p.tool_response });
}
main().catch(() => {}).finally(() => process.exit(0));
