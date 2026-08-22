#!/usr/bin/env node
// Stop: the LOAD-BEARING episode flush (Codex's SessionEnd gets a 3 s hard
// kill, so the flush cannot live there), plus the once-per-session capture
// nudge. Substantive-session signal: the session's own spooled codex tool
// artifacts — there is no parseable Codex transcript yet (rollout format is
// promoted from live payloads later, like the fixtures). HARD RULES: no
// daemon → episode state kept for a later sweep, never dropped; a
// stop_hook_active follow-up turn is silent AND never burns the nudge
// marker; exit 0 no matter what.
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { warehouseDir } from "../core/warehouse-spool.js";
import { flushEpisode } from "../core/hooks/episode.js";
import { nudgeGate, NUDGE_TEXT } from "../core/hooks/stop.js";
import { readState, markNudged } from "../core/recorder.js";
import { hookContext, HARNESS } from "./_env.js";

/** Count this session's spooled codex tool artifacts across all spool files
 *  (the file name carries the writer's pid, so one session spans many). */
function spooledToolUses(dataDir, sessionId, env) {
  const dir = path.join(warehouseDir(dataDir, env), "spool");
  let names;
  try { names = readdirSync(dir); } catch { return 0; }
  let n = 0;
  for (const name of names) {
    let lines;
    try { lines = readFileSync(path.join(dir, name), "utf8").split("\n"); } catch { continue; }
    for (const line of lines) {
      if (!line) continue;
      try {
        const e = JSON.parse(line);
        if (e.kind === "artifact" && e.source?.harness === HARNESS && e.source?.session === String(sessionId)) n++;
      } catch { /* torn tail line: not a countable artifact */ }
    }
  }
  return n;
}

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "Stop", raw });
  if (!p) return;
  if (!dataDir || !projectDir || !sessionId) return;
  // Read state BEFORE the flush: a successful (or empty) flush deletes the
  // state file, and the nudge decision needs the pre-flush nudged/captured flags.
  const state = readState(dataDir, sessionId);
  const assistantText = typeof p.last_assistant_message === "string" ? p.last_assistant_message : null;
  await flushEpisode({ dataDir, env: process.env, projectDir, sessionId, assistantText, reason: "stop" });
  // The flush's state cleanup must not un-set once-per-session markers.
  if (state?.nudged === true) markNudged(dataDir, sessionId);
  if (p.stop_hook_active === true || p.stop_hook_active === "true") return; // auto-submitted follow-up turn
  const toolUses = spooledToolUses(dataDir, sessionId, process.env);
  if (!nudgeGate({ dataDir, env: process.env, sessionId, state, toolUses })) return;
  markNudged(dataDir, sessionId);
  process.stdout.write(JSON.stringify({ decision: "block", reason: NUDGE_TEXT }));
}
runHook(main, { deadlineMs: 600000 });
