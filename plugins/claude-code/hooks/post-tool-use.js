#!/usr/bin/env node
// PostToolUse (matched to topodb retrieval tools): append what the model
// just retrieved to this session's episode state file. No broker contact,
// no stdout, exit 0 always.
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { toRetrievalRecord, appendRetrieval, normalizeToolResult } from "../recorder.js";

function recordingDisabled(env) {
  const v = (env.TOPODB_RECORDING ?? "").toLowerCase();
  return v === "0" || v === "off";
}

/** Debug escape (TOPODB_HOOK_DEBUG=1): dump the raw stdin payload so the
 * first real session can pin the true PostToolUse tool_output/tool_response
 * shape. Best-effort — never throws, never writes to stdout. */
function dumpDebugPayload(dataDir, raw) {
  try {
    const dir = path.join(dataDir, "episodes");
    mkdirSync(dir, { recursive: true });
    writeFileSync(path.join(dir, "debug-last-payload.json"), raw);
  } catch { /* best-effort only */ }
}

async function main() {
  const raw = await new Promise((r) => {
    let buf = "";
    process.stdin.on("data", (d) => (buf += d));
    process.stdin.on("end", () => r(buf));
  });
  // Debug escape, before anything else that could bail out early.
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (process.env.TOPODB_HOOK_DEBUG && dataDir) {
    dumpDebugPayload(dataDir, raw);
  }
  if (recordingDisabled(process.env)) return;
  let p;
  try {
    p = JSON.parse(raw);
  } catch {
    return;
  }
  if (p.agent_id || p.agent_type) return; // main sessions only
  if (!dataDir || !p.session_id) return;

  // mcp__plugin_topodb_topodb__search_memories -> search_memories
  const tool = String(p.tool_name ?? "").split("__").pop();
  const rawResult = p.tool_output ?? p.tool_response; // docs carry both names
  const result = normalizeToolResult(rawResult);
  const rec = toRetrievalRecord(tool, p.tool_input ?? {}, result);
  if (!rec) return;
  appendRetrieval(dataDir, p.session_id, rec.record, rec.contents);
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
