#!/usr/bin/env node
// postToolUse: land the raw artifact into the warehouse spool. Cursor tool
// names are mapped by core/tool-map.js; MCP and unknown tools are dropped
// (never guessed). Subagent tool calls are captured and attributed to the
// session (subagent_id when present). No daemon, no stdout, exit 0 always.
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { fromCursorToolUse } from "../core/tool-map.js";
import { captureArtifact } from "../core/hooks/capture.js";
import { hookContext, HARNESS } from "./_env.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "postToolUse", raw });
  if (!p) return;
  const mapped = fromCursorToolUse(p);
  if (!mapped) return;
  captureArtifact({ dataDir, env: process.env, projectDir, sessionId, ...mapped, cwd: p.cwd, agent: p.subagent_id, harness: HARNESS });
}
runHook(main, { deadlineMs: 4000 });
