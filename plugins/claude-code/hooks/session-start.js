#!/usr/bin/env node
// SessionStart: inject the project's recent memories as context.
// HARD RULES: exit 0 no matter what; nothing on stdout except the payload;
// self-deadline (this hook BLOCKS session start); main sessions only.
import { pathToFileURL } from "node:url";
import { connectForProject } from "../broker-client.js";

const DEADLINE_MS = 2500;
const K = 10;
const KEEP = 8;
const CHAR_CAP = 6000;
// Session-start hygiene glance: run all three scans in one call. A 90-day
// staleness window keeps the "stale" nudge meaningful (genuinely cold, not
// routine 30-day-old memories that would nag every session). The health call
// overlaps the enrichment loop (concurrent, own timeout), so it adds no serial
// latency to the memory injection — and is never load-bearing.
const HEALTH_STALE_DAYS = 90;
const HEALTH_TIMEOUT_MS = 1200;

// A one-line hygiene nudge for the categories that are non-zero, or null when
// the store is tidy (or health is unavailable). Advisory — points at the tools,
// never acts.
export function renderHealth(health) {
  if (!health || !health.needs_attention) return null;
  const plur = (n, s) => `${n} ${s}${n === 1 ? "" : "s"}`;
  const parts = [];
  if (health.duplicate_pairs > 0) parts.push(plur(health.duplicate_pairs, "duplicate pair"));
  if (health.supersession_pairs > 0) parts.push(plur(health.supersession_pairs, "supersession"));
  if (health.orphan_count > 0) parts.push(plur(health.orphan_count, "orphan"));
  if (health.stale_count > 0) parts.push(`${health.stale_count} stale`);
  if (!parts.length) return null;
  return `🧹 Memory hygiene: ${parts.join(", ")} — review with memory_health, then consolidate/link/supersede.`;
}

export function renderInjection(memories, healthLine = null) {
  if (!memories.length) return null;
  const lines = ["## TopoDB memory (this project)"];
  for (const m of memories) {
    const content = m.content.length > 140 ? `${m.content.slice(0, 139)}…` : m.content;
    const ents = m.entities.length ? ` [entities: ${m.entities.slice(0, 3).join(", ")}]` : "";
    const days = Math.floor(m.ageMs / 86400000);
    const age = days > 0 ? ` (${days}d ago)` : " (today)";
    const line = `- ${content}${ents}${age}`;
    if (lines.join("\n").length + line.length > CHAR_CAP) break;
    lines.push(line);
  }
  if (healthLine) lines.push(healthLine);
  lines.push("Deeper recall: search_memories / traverse. Store: remember.");
  return lines.join("\n");
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
  if (payload.agent_id || payload.agent_type) return; // main sessions only
  if (payload.source !== "startup" && payload.source !== "clear") return;

  const dataDir = process.env.CLAUDE_PLUGIN_DATA;
  const projectDir = process.env.CLAUDE_PROJECT_DIR ?? payload.cwd;
  if (!dataDir || !projectDir) return;

  const client = await connectForProject({ projectDir, dataDir });
  if (!client) return; // no broker yet — first-ever session; next one has it
  try {
    const recent = await client.call("recent_memories", { k: K });
    const nodes = Array.isArray(recent.memories) ? recent.memories : [];
    if (!nodes.length) return;

    // Fire the health scan concurrently so it overlaps the enrichment loop
    // below (the broker multiplexes by request id). Own timeout + swallow: a
    // hygiene nudge is a nicety, never worth risking the memory injection.
    const healthPromise = client
      .call("memory_health", { stale_older_than_days: HEALTH_STALE_DAYS }, HEALTH_TIMEOUT_MS)
      .catch(() => null);

    const enriched = [];
    for (const n of nodes) {
      if (typeof n?.id !== "string" || typeof n?.props?.content !== "string") continue;
      let accessCount = 0;
      try {
        const stats = await client.call("access_stats", { id: n.id }, 800);
        if (stats.found && typeof stats.access_count === "number") accessCount = stats.access_count;
      } catch { /* stats are a ranking nicety, never load-bearing */ }
      const entities = [];
      try {
        const edges = await client.call("get_edges", { from_id: n.id }, 800);
        for (const e of (edges.edges ?? []).slice(0, 3)) {
          try {
            const t = await client.call("get_node", { id: e.to }, 800);
            const name = t?.node?.props?.name;
            if (typeof name === "string") entities.push(name);
          } catch { /* skip */ }
        }
      } catch { /* entity names are decoration */ }
      // ULID timestamp: first 10 chars are Crockford-base32 time — cheap
      // decode not worth it; approximate age from access stats' last read
      // is wrong too. Use 0 and render "today" rather than decode ULIDs.
      enriched.push({ id: n.id, content: n.props.content, entities, ageMs: 0, accessCount });
    }
    enriched.sort((a, b) => b.accessCount - a.accessCount);
    const healthLine = renderHealth(await healthPromise);
    const out = renderInjection(enriched.slice(0, KEEP), healthLine);
    if (out) {
      // CHAR_CAP (6000) keeps this comfortably under the ~64KB pipe buffer,
      // so the process.exit(0) in finally() below can never truncate the
      // write mid-flight. Revisit this assumption if CHAR_CAP grows a lot.
      process.stdout.write(
        JSON.stringify({
          hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: out },
        }),
      );
    }
  } finally {
    client.close();
  }
}

// Only run main() when executed as a script — the test imports renderInjection.
// Compared via pathToFileURL (not a hand-built `file://` template) so this
// holds for paths with spaces and on Windows, where a template string
// mismatches the URL's percent-encoding/drive-letter form.
if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const guard = setTimeout(() => process.exit(0), DEADLINE_MS);
  main()
    .catch(() => {})
    .finally(() => {
      clearTimeout(guard);
      process.exit(0);
    });
}
