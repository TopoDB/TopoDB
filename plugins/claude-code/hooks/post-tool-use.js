#!/usr/bin/env node
// PostToolUse (matched to topodb retrieval tools): append what the model
// just retrieved to this session's episode state file. No broker contact,
// no stdout, exit 0 always.
import { toRetrievalRecord, appendRetrieval } from "../recorder.js";

function recordingDisabled(env) {
  const v = (env.TOPODB_RECORDING ?? "").toLowerCase();
  return v === "0" || v === "off";
}

async function main() {
  if (recordingDisabled(process.env)) return;
  const raw = await new Promise((r) => {
    let buf = "";
    process.stdin.on("data", (d) => (buf += d));
    process.stdin.on("end", () => r(buf));
  });
  let p;
  try {
    p = JSON.parse(raw);
  } catch {
    return;
  }
  if (p.agent_id || p.agent_type) return; // main sessions only
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir || !p.session_id) return;

  // mcp__plugin_topodb_topodb__search_memories -> search_memories
  const tool = String(p.tool_name ?? "").split("__").pop();
  const result = p.tool_output ?? p.tool_response; // docs carry both names
  const rec = toRetrievalRecord(tool, p.tool_input ?? {}, result);
  if (!rec) return;
  appendRetrieval(dataDir, p.session_id, rec.record, rec.contents);
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
