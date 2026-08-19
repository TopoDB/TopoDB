#!/usr/bin/env node
// sessionEnd: flush the session's episode through the daemon and clean up.
// If Cursor's transcript shape is unknown, the episode is still written but
// usage is NOT judged (usage_judged=false) — never a guessed judgment.
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { parseCursor, readTranscript, assistantTextOrNull } from "../core/transcript.js";
import { flushEpisode } from "../core/hooks/episode.js";
import { hookContext, HARNESS } from "./_env.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "sessionEnd", raw });
  if (!p || p.is_background_agent === true) return;
  if (!dataDir || !projectDir || !sessionId) return;
  tryMarker({ dataDir, env: process.env, projectDir, sessionId, type: "session_end", sessionScopes, harness: HARNESS });
  const text = readTranscript(p.transcript_path ?? process.env.CURSOR_TRANSCRIPT_PATH);
  const assistantText = text === null ? null : assistantTextOrNull(parseCursor(text));
  const r = await flushEpisode({ dataDir, env: process.env, projectDir, sessionId, assistantText, reason: p.reason });
  if (r === "no-daemon") console.error("topodb hooks: daemon gone at session end; episode kept for a later sweep");
  if (r === "failed") console.error("topodb hooks: episode flush failed");
}
runHook(main, { deadlineMs: 8000 });
