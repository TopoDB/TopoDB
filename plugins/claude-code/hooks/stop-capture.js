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
