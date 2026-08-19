#!/usr/bin/env node
// PostToolUse (memory write tools): flag that the agent already saved this
// session, so the Stop capture-nudge does not fire. Main sessions only.
import { readStdin, parseJson, recordingDisabled } from "../core/hook-io.js";
import { recordMemoryWrite } from "../core/hooks/retrieval.js";

async function main() {
  const raw = await readStdin();
  if (recordingDisabled(process.env)) return;
  const p = parseJson(raw); if (!p) return;
  if (p.agent_id || p.agent_type) return;
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir || !p.session_id) return;
  recordMemoryWrite({ dataDir, env: process.env, projectDir: process.env.CLAUDE_PROJECT_DIR ?? p.cwd, sessionId: p.session_id,
    toolResult: p.tool_response ?? p.tool_output, harness: "claude-code" });
}
main().catch(() => {}).finally(() => process.exit(0));
