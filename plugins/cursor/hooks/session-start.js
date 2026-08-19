#!/usr/bin/env node
// sessionStart: inject the project's recent memories as context and hand the
// session id to later hooks via `env`. Main (non-background) sessions only.
// HARD RULES: exit 0 no matter what; nothing on stdout except the payload;
// self-deadline.
import { readStdin, parseJson, debugDump, runHook } from "../core/hook-io.js";
import { connectForProject } from "../core/broker-client.js";
import { recallForSessionStart } from "../core/hooks/recall.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { hookContext, HARNESS } from "./_env.js";

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "sessionStart", raw });
  if (!p || p.is_background_agent === true) return;
  if (!dataDir || !projectDir || !sessionId) return;
  tryMarker({ dataDir, env: process.env, projectDir, sessionId, type: "session_start", sessionScopes, harness: HARNESS });
  const out = { env: { TOPODB_SESSION_ID: String(sessionId) } };
  const client = await connectForProject({ projectDir, dataDir });
  if (client) {
    try { const text = await recallForSessionStart(client); if (text) out.additional_context = text; }
    catch { /* first-ever session or daemon hiccup: inject nothing */ }
    finally { client.close(); }
  }
  process.stdout.write(JSON.stringify(out));
}
runHook(main, { deadlineMs: 2500 });
