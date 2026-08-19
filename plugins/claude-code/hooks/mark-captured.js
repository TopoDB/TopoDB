#!/usr/bin/env node
// PostToolUse (memory write tools): flag that the agent already saved this
// session, so the Stop capture-nudge does not fire. Main sessions only.
import { markCaptured, normalizeToolResult } from "../core/recorder.js";
import { sessionScopes } from "../core/server-args.js";
import { tryMarker } from "../core/warehouse-spool.js";

function recordingDisabled(env) {
  const v = (env.TOPODB_RECORDING ?? "").toLowerCase();
  return v === "0" || v === "off";
}

async function main() {
  const raw = await new Promise((r) => {
    let buf = "";
    process.stdin.on("data", (d) => (buf += d));
    process.stdin.on("end", () => r(buf));
  });
  if (recordingDisabled(process.env)) return;
  let p;
  try {
    p = JSON.parse(raw);
  } catch {
    return;
  }
  if (p.agent_id || p.agent_type) return; // main sessions only
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir || !p.session_id) return;
  markCaptured(dataDir, p.session_id);
  const r = normalizeToolResult(p.tool_response ?? p.tool_output) ?? {};
  const ids = [...new Set([r.memory_id, r.id, r.node?.id, r.memory?.id].filter((s) => typeof s === "string" && s.length === 26))];
  if (ids.length) tryMarker({ dataDir, env: process.env, projectDir: process.env.CLAUDE_PROJECT_DIR ?? p.cwd, sessionId: p.session_id, type: "memory_write", nodeIds: ids, sessionScopes });
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
