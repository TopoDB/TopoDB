#!/usr/bin/env node
// stop: nudge the agent to save durable memories before finishing — once per
// session, hard-gated, and only on a clean completion. Cursor's followup_message
// AUTO-SUBMITS a turn, so the gate + hooks.json loop_limit:1 + the per-session
// `nudged` marker are all load-bearing. Fail-open: any doubt → no output.
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { parseCursor, readTranscript } from "../core/transcript.js";
import { nudgeGate, NUDGE_TEXT } from "../core/hooks/stop.js";
import { readState, markNudged } from "../core/recorder.js";
import { hookContext } from "./_env.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "stop", raw });
  if (!p || p.status !== "completed") return;
  if (Number(p.loop_count ?? 0) > 0) return;
  if (!dataDir || !sessionId) return;
  const text = readTranscript(p.transcript_path ?? process.env.CURSOR_TRANSCRIPT_PATH);
  const toolUses = text === null ? 0 : parseCursor(text).toolUses;
  const state = readState(dataDir, sessionId);
  if (!nudgeGate({ dataDir, env: process.env, sessionId, state, toolUses })) return;
  markNudged(dataDir, sessionId);
  process.stdout.write(JSON.stringify({ followup_message: NUDGE_TEXT }));
}
runHook(main, { deadlineMs: 1500 });
