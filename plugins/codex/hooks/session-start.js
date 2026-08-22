#!/usr/bin/env node
// SessionStart: inject the project's recent memories as context. Fires on
// startup|resume|clear AND on compact — the compact re-inject is the point
// (recall must survive compaction), so unlike claude-code's hook this script
// never gates on `source`. HARD RULES: exit 0 no matter what; nothing on
// stdout except the payload; self-deadline; main (non-background) sessions only.
import { pathToFileURL } from "node:url";
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { connectForProject } from "../core/broker-client.js";
import { recallForSessionStart } from "../core/hooks/recall.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { hookContext, HARNESS } from "./_env.js";

// Clamp for additionalContext: above recall's own 6,000-char CHAR_CAP (a
// normal injection passes through untouched), under Codex's ~2,500-token
// (~10,000-char) spill-to-file threshold.
export const ADDITIONAL_CONTEXT_LIMIT = 8000;

export function buildOutput(text) {
  if (typeof text !== "string" || text === "") return null;
  const clamped = text.length > ADDITIONAL_CONTEXT_LIMIT ? text.slice(0, ADDITIONAL_CONTEXT_LIMIT) : text;
  return { hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: clamped } };
}

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "SessionStart", raw });
  if (!p || p.is_background_agent === true) return;
  if (!dataDir || !projectDir || !sessionId) return;
  tryMarker({ dataDir, env: process.env, projectDir, sessionId, type: "session_start", sessionScopes, harness: HARNESS });
  const client = await connectForProject({ projectDir, dataDir });
  if (!client) return; // no broker yet — first-ever session; next one has it
  try {
    const out = buildOutput(await recallForSessionStart(client));
    if (out) process.stdout.write(JSON.stringify(out));
  } catch { /* empty store or daemon hiccup: inject nothing */ }
  finally { client.close(); }
}

// Only run main() when executed as a script — the test imports buildOutput.
if (import.meta.url === pathToFileURL(process.argv[1]).href) runHook(main, { deadlineMs: 2500 });
