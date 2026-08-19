#!/usr/bin/env node
// Stop: nudge the main agent to save durable memories before finishing —
// once per session, hard-gated. HARD RULES: fail-open (allow the stop) on any
// doubt; self-deadline; never throw; never nudge more than once per session.
import { pathToFileURL } from "node:url";
import { parseClaude, readTranscript } from "../core/transcript.js";
import { nudgeGate, NUDGE_TEXT } from "../core/hooks/stop.js";
import { readState, markNudged } from "../core/recorder.js";
import { readStdin, parseJson } from "../core/hook-io.js";

export function countToolUses(transcriptText) { return parseClaude(transcriptText).toolUses; }
export function decideNudge({ payload, env, state, toolUses }) {
  if (!payload) return false;
  if (payload.stop_hook_active === true || payload.stop_hook_active === "true") return false;
  if (payload.agent_id || payload.agent_type) return false;
  if (payload.permission_mode === "plan") return false;
  return nudgeGate({ dataDir: env.CLAUDE_PLUGIN_DATA, env, sessionId: payload.session_id, state, toolUses });
}

const DEADLINE_MS = 1500;

async function main() {
  const raw = await readStdin();
  const payload = parseJson(raw);
  if (!payload) return;
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  const state = dataDir && payload.session_id ? readState(dataDir, payload.session_id) : null;
  const toolUses = countToolUses(readTranscript(payload.transcript_path));
  if (!decideNudge({ payload, env: process.env, state, toolUses })) return;
  markNudged(dataDir, payload.session_id);
  process.stdout.write(JSON.stringify({ decision: "block", reason: NUDGE_TEXT }));
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const guard = setTimeout(() => process.exit(0), DEADLINE_MS);
  main()
    .catch(() => {})
    .finally(() => {
      clearTimeout(guard);
      process.exit(0);
    });
}
