#!/usr/bin/env node
// SessionEnd: flush the session's episode (if any) through the daemon, then
// clean up. Sweeps stale state files from crashed sessions. No stdout, exit 0.
import { readStdin, parseJson } from "../core/hook-io.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";
import { parseClaude, readTranscript, assistantTextOrNull } from "../core/transcript.js";
import { flushEpisode } from "../core/hooks/episode.js";

async function main() {
  const p = parseJson(await readStdin()); if (!p) return;
  if (p.agent_id || p.agent_type) return;
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir) return;
  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? p.cwd;
  tryMarker({ dataDir, env: process.env, projectDir, sessionId: p.session_id, type: "session_end", sessionScopes, harness: "claude-code" });
  const text = p.transcript_path ? readTranscript(p.transcript_path) : null;
  const assistantText = text === null ? "" : assistantTextOrNull(parseClaude(text)) ?? "";
  const r = await flushEpisode({ dataDir, env: process.env, projectDir, sessionId: p.session_id, assistantText, reason: p.reason });
  if (r === "no-daemon") console.error("topodb hooks: daemon gone at session end; episode dropped");
}
main().catch(() => {}).finally(() => process.exit(0));
