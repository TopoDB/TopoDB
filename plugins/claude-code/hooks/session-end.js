#!/usr/bin/env node
// SessionEnd: flush the session's episode (if any) through the broker,
// then clean up. Sweeps stale state files from crashed sessions. No
// stdout, exit 0 always.
import { readFileSync } from "node:fs";
import { connectForProject } from "../broker-client.js";
import { readState, deleteState, sweepStale, extractText, isUsed, buildEpisodeBatch } from "../recorder.js";

function recordingDisabled(env) {
  const v = (env.TOPODB_RECORDING ?? "").toLowerCase();
  return v === "0" || v === "off";
}

/** All assistant text in a Claude Code transcript JSONL. Defensive: skip
 * unparseable lines; only `type === "assistant"` entries contribute. */
function assistantText(transcriptPath) {
  let raw;
  try {
    raw = readFileSync(transcriptPath, "utf8");
  } catch {
    return "";
  }
  const parts = [];
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    try {
      const entry = JSON.parse(line);
      if (entry?.type === "assistant") parts.push(extractText(entry?.message?.content));
    } catch { /* skip */ }
  }
  return parts.join("\n");
}

async function main() {
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
  if (p.agent_id || p.agent_type) return;
  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  if (!dataDir) return;

  sweepStale(dataDir); // crashed sessions' leftovers, any time we're here

  if (recordingDisabled(process.env) || !p.session_id) return;
  const state = readState(dataDir, p.session_id);
  if (!state || !state.retrievals.length) {
    deleteState(dataDir, p.session_id);
    return;
  }

  const text = p.transcript_path ? assistantText(p.transcript_path) : "";
  const used = new Map();
  state.retrievals.forEach((r, i) => {
    const ids = new Set();
    for (const m of r.returned) {
      const content = state.contents[m.id];
      if (content && text && isUsed(content, text)) ids.add(m.id);
    }
    if (ids.size) used.set(i, ids);
  });

  const cmds = buildEpisodeBatch({
    state,
    outcome: "success",
    failure: "",
    endedAt: Date.now(),
    used,
  });

  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? p.cwd;
  const client = await connectForProject({ projectDir, dataDir });
  if (!client) {
    console.error("topodb hooks: broker gone at session end; episode dropped");
    return; // state file left for a later sweep — better than losing it silently now
  }
  try {
    await client.call("submit_batch", { commands: cmds }, 5000);
    deleteState(dataDir, p.session_id);
  } catch (e) {
    console.error(`topodb hooks: episode flush failed: ${e.message}`);
  } finally {
    client.close();
  }
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
