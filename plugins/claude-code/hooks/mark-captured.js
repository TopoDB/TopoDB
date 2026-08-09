#!/usr/bin/env node
// PostToolUse (memory write tools): flag that the agent already saved this
// session, so the Stop capture-nudge does not fire. Main sessions only.
import { markCaptured } from "../recorder.js";

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
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
