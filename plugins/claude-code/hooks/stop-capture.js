#!/usr/bin/env node
// Stop: nudge the main agent to save durable memories before finishing —
// once per session, hard-gated. HARD RULES: fail-open (allow the stop) on any
// doubt; self-deadline; never throw; never nudge more than once per session.

const SUBSTANTIVE_MIN_TOOLS = 5;

function offSwitch(v) {
  const s = String(v ?? "").toLowerCase();
  return s === "off" || s === "0";
}

// Count tool_use blocks across assistant turns in a Claude Code transcript.
export function countToolUses(transcriptText) {
  if (!transcriptText) return 0;
  let n = 0;
  for (const line of transcriptText.split("\n")) {
    if (!line.trim()) continue;
    let obj;
    try {
      obj = JSON.parse(line);
    } catch {
      continue;
    }
    if (obj?.type !== "assistant") continue;
    const c = obj?.message?.content;
    if (Array.isArray(c)) for (const item of c) if (item?.type === "tool_use") n++;
  }
  return n;
}

// True only when it is safe and warranted to nudge. Pure — all inputs passed in.
export function decideNudge({ payload, env, state, toolUses }) {
  if (!payload) return false;
  if (payload.stop_hook_active === true || payload.stop_hook_active === "true") return false;
  if (payload.agent_id || payload.agent_type) return false;
  if (payload.permission_mode === "plan") return false;
  if (!env.CLAUDE_PLUGIN_DATA) return false;
  if (offSwitch(env.TOPODB_RECORDING) || offSwitch(env.TOPODB_CAPTURE_NUDGE)) return false;
  if (!payload.session_id) return false;
  if (state?.nudged === true) return false;
  if (state?.captured === true) return false;
  if (toolUses < SUBSTANTIVE_MIN_TOOLS) return false;
  return true;
}

import { pathToFileURL } from "node:url";
import { readFileSync } from "node:fs";
import { readState, markNudged } from "../core/recorder.js";

const DEADLINE_MS = 1500;
const NUDGE_TEXT =
  "This session may have produced durable facts, decisions, or lessons. Before finishing, save anything worth keeping across sessions with the `remember` tool (project scope by default; `shared` if it generalizes), and `supersede` any memory this session made outdated. If nothing durable came out of this session, just stop — do not save trivia or restate what is already in memory.";

function readMaybe(p) {
  try {
    return readFileSync(p, "utf8");
  } catch {
    return null;
  }
}

async function main() {
  const raw = await new Promise((r) => {
    let buf = "";
    process.stdin.on("data", (d) => (buf += d));
    process.stdin.on("end", () => r(buf));
  });
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    return;
  }
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  const state = dataDir && payload.session_id ? readState(dataDir, payload.session_id) : null;
  const toolUses = countToolUses(readMaybe(payload.transcript_path));
  if (!decideNudge({ payload, env: process.env, state, toolUses })) return;
  markNudged(dataDir, payload.session_id);
  process.stdout.write(JSON.stringify({ decision: "block", reason: NUDGE_TEXT }));
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const guard = setTimeout(() => process.exit(0), DEADLINE_MS);
  main()
    .catch(() => {})
    .finally(() => {
      clearTimeout(guard);
      process.exit(0);
    });
}
