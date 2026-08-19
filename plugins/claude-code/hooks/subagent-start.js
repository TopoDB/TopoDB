#!/usr/bin/env node
// SubagentStart: prime a dispatched subagent with the memories relevant to its
// task. HARD RULES: exit 0 no matter what; nothing on stdout except the
// injection payload; self-deadline; never break or delay a dispatch.
import { renderMemoryLines } from "../core/render.js";
import { pathToFileURL } from "node:url";
import { readFileSync } from "node:fs";
import { connectForProject } from "../core/broker-client.js";

const QUERY_CAP = 1000;
const CHAR_CAP = 2000;
const DEFAULT_SKIP = ["explore", "plan"];

// The first user turn of a subagent's transcript is its dispatched task. Flatten
// that message to a search query; null if there's no clean task to search by.
export function extractTask(transcriptText) {
  if (!transcriptText) return null;
  for (const line of transcriptText.split("\n")) {
    if (!line.trim()) continue;
    let obj;
    try {
      obj = JSON.parse(line);
    } catch {
      continue;
    }
    if (obj?.type !== "user" || obj?.message?.role !== "user") continue;
    const c = obj.message.content;
    let text = "";
    if (typeof c === "string") text = c;
    else if (Array.isArray(c)) text = c.map((p) => (typeof p?.text === "string" ? p.text : "")).join("");
    text = text.trim();
    return text ? text.slice(0, QUERY_CAP) : null;
  }
  return null;
}

// Agent-type names to skip, lowercased. TOPODB_SUBAGENT_SKIP (comma-separated)
// replaces the default when defined; an empty string skips nothing.
export function skipSet(env) {
  const raw = env.TOPODB_SUBAGENT_SKIP;
  const names = raw === undefined ? DEFAULT_SKIP : raw.split(",");
  return new Set(names.map((s) => s.trim().toLowerCase()).filter(Boolean));
}

// The injection body from search_memories hits, or null when there's nothing
// usable to inject. Filters to Memory nodes only (not Entity links).
export function renderSubagentContext(hits) {
  const mems = [];
  for (const h of hits) {
    if (h?.label !== "Memory") continue;
    const content = h?.props?.content;
    if (typeof content === "string" && content.trim()) mems.push({ content, entities: [], ageMs: 0 });
  }
  if (!mems.length) return null;
  const lines = renderMemoryLines(mems, "## Relevant project memory", CHAR_CAP);
  lines.push("Recall more: search_memories. Store: remember.");
  return lines.join("\n");
}

const DEADLINE_MS = 2000;
const K = 5;
const SEARCH_TIMEOUT_MS = 1500;

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
  if (!payload.agent_id) return; // subagent events only
  if (skipSet(process.env).has(String(payload.agent_type ?? "").toLowerCase())) return;

  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? payload.cwd;
  if (!dataDir || !projectDir) return;

  const task = extractTask(readMaybe(payload.transcript_path));
  if (!task) return;

  const client = await connectForProject({ projectDir, dataDir });
  if (!client) return;
  try {
    // temporal_rewrite: false — this query is machine-built from raw prompt text,
    // not a human temporal question. A dated plan path in the task (e.g.
    // "docs/.../2026-08-09-foo.md") would otherwise time-box this priming
    // search to that single day and silently return nothing.
    const res = await client.call(
      "search_memories",
      { query: task, k: K, labels: ["Memory"], temporal_rewrite: false },
      SEARCH_TIMEOUT_MS,
    );
    const hits = Array.isArray(res?.hits) ? res.hits.map(h => h?.node).filter(Boolean) : [];
    const out = renderSubagentContext(hits);
    if (out) {
      process.stdout.write(
        JSON.stringify({ hookSpecificOutput: { hookEventName: "SubagentStart", additionalContext: out } }),
      );
    }
  } catch {
    /* recall is best-effort; a dispatch is never blocked on it */
  } finally {
    client.close();
  }
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
