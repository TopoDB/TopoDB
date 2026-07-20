// recorder.js — pure episode-recording core: no external imports except node fs/path
// Everything here is deterministic and unit-tested.

import { mkdirSync, readFileSync, writeFileSync, unlinkSync, renameSync, readdirSync, statSync, existsSync } from "node:fs";
import path from "node:path";

// State-file helpers (replacing EpisodeBuffer class)

export function stateFilePath(dataDir, sessionId) {
  // sessionId is used as a filename: strip anything path-like defensively.
  return path.join(dataDir, "episodes", `${String(sessionId).replace(/[^A-Za-z0-9._-]/g, "_")}.json`);
}

export function appendRetrieval(dataDir, sessionId, record, contents) {
  const file = stateFilePath(dataDir, sessionId);
  mkdirSync(path.dirname(file), { recursive: true });
  const state = readState(dataDir, sessionId) ?? { startedAt: Date.now(), retrievals: [], contents: {} };
  state.retrievals.push(record);
  for (const [id, text] of contents) state.contents[id] = text;
  // Atomic replace: write to a per-process temp file in the same dir, then
  // rename over the final path. A concurrent PostToolUse racing us can still
  // lose an update (last rename wins), but readers never observe a torn
  // (partially-written) file — renameSync is atomic on the same filesystem.
  const tmp = `${file}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(state));
  // On Windows, renaming onto a target another process is concurrently
  // replacing (or reading) fails EPERM/EACCES — observed with concurrent
  // PostToolUse hooks. Bounded linear backoff, synchronous because every
  // hook calls this synchronously (Atomics.wait sleep, the launch.js
  // install-lock precedent). Last rename still wins; a rename that lands
  // is still atomic, so readers never see a torn file.
  const sleeper = new Int32Array(new SharedArrayBuffer(4));
  for (let attempt = 0; ; attempt++) {
    try {
      renameSync(tmp, file);
      return;
    } catch (err) {
      if ((err.code !== "EPERM" && err.code !== "EACCES") || attempt >= 20) {
        try {
          unlinkSync(tmp); // don't strand the temp file behind a throw
        } catch {}
        throw err;
      }
      Atomics.wait(sleeper, 0, 0, 5 + attempt * 5);
    }
  }
}

export function readState(dataDir, sessionId) {
  const file = stateFilePath(dataDir, sessionId);
  if (!existsSync(file)) return null;
  try {
    const state = JSON.parse(readFileSync(file, "utf8"));
    if (!Array.isArray(state.retrievals)) throw new Error("shape");
    return state;
  } catch {
    console.error(`topodb hooks: discarding malformed state file ${file}`);
    try { unlinkSync(file); } catch { /* already gone */ }
    return null;
  }
}

export function deleteState(dataDir, sessionId) {
  try { unlinkSync(stateFilePath(dataDir, sessionId)); } catch { /* fine */ }
}

export function sweepStale(dataDir, maxAgeMs = 7 * 24 * 3600 * 1000) {
  const dir = path.join(dataDir, "episodes");
  let names;
  try { names = readdirSync(dir); } catch { return; }
  const cutoff = Date.now() - maxAgeMs;
  for (const n of names) {
    const p = path.join(dir, n);
    try { if (statSync(p).mtimeMs < cutoff) unlinkSync(p); } catch { /* races are fine */ }
  }
}

// Text helpers

/** Message content -> plain text: strings pass through, block arrays
 * contribute only their `type === "text"` blocks. Defensive: anything
 * unrecognized contributes nothing. */
export function extractText(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((b) =>
      b && typeof b === "object" && b.type === "text"
        ? String(b.text ?? "")
        : "",
    )
    .join("");
}

/** Spec tokenization: lowercase alphanumeric runs of length >= 3, deduped,
 * insertion order preserved. */
export function tokenize(text) {
  const out = new Set();
  for (const m of text.toLowerCase().matchAll(/[a-z0-9]{3,}/g)) out.add(m[0]);
  return out;
}

/** Spec USED rule: >= 50% of the memory's tokens appear in the text. A
 * memory with no tokens is never "used". */
export function isUsed(memContent, text) {
  const mem = tokenize(memContent);
  if (mem.size === 0) return false;
  const hay = tokenize(text);
  let hits = 0;
  for (const t of mem) if (hay.has(t)) hits++;
  return hits / mem.size >= 0.5;
}

/** Normalize a raw PostToolUse `tool_output`/`tool_response` payload into the
 * parsed structured result `toRetrievalRecord` expects. Real MCP hook
 * payloads may deliver: a `structuredContent` object; a content-block array
 * (or an envelope with a `content` array) whose first `{type:"text"}` block
 * holds a JSON string; or (today's tests) the already-parsed object. Never
 * throws — returns undefined on anything it can't make sense of. */
export function normalizeToolResult(raw) {
  if (raw === null || raw === undefined) return undefined;
  if (typeof raw === "object" && !Array.isArray(raw) && raw.structuredContent && typeof raw.structuredContent === "object") {
    return raw.structuredContent;
  }
  const blocks = Array.isArray(raw) ? raw : Array.isArray(raw?.content) ? raw.content : null;
  if (blocks) {
    const textBlock = blocks.find((b) => b && typeof b === "object" && b.type === "text");
    if (!textBlock || typeof textBlock.text !== "string") return undefined;
    try {
      return JSON.parse(textBlock.text);
    } catch {
      return undefined;
    }
  }
  return raw;
}

// Retrieval record building

/** Helper to collect a node into the returned list and contents map. */
function collect(node, i, score, out, contents) {
  const id = node?.id;
  if (typeof id !== "string") return;
  out.push({ id, rank: i, score });
  const content = node?.props?.content;
  if (typeof content === "string") contents.set(id, content);
}

/** Map a `search_memories`/`traverse`/`recent_memories` tool result to a `RetrievalRecord` plus
 * the memory contents it surfaced, or `undefined` when the tool isn't a
 * retrieval call or the result doesn't match the expected wire shape (never
 * throws — recording must never break the agent). Field names follow
 * topodb-mcp's actual JSON (captured from the running server, not guessed):
 * `search_memories` -> `{hits: [{node, score}]}`; `traverse` -> `{subgraph:
 * {nodes, edges}}`; `recent_memories` -> `{memories: [node…]}`. */
export function toRetrievalRecord(tool, args, result) {
  const contents = new Map();
  const returned = [];

  if (tool === "search_memories") {
    const hits = result?.hits;
    if (!Array.isArray(hits)) return undefined;
    hits.forEach((h, i) => {
      const score = typeof h?.score === "number" ? h.score : 0;
      collect(h?.node, i, score, returned, contents);
    });
    const query = typeof args.query === "string" ? args.query : "";
    return { record: { query, at: Date.now(), channel: "text", returned }, contents };
  }

  if (tool === "traverse") {
    const nodes = result?.subgraph?.nodes;
    if (!Array.isArray(nodes)) return undefined;
    nodes.forEach((n, i) => collect(n, i, 0, returned, contents));
    const query = typeof args.seed_id === "string" ? args.seed_id : "";
    return { record: { query, at: Date.now(), channel: "graph", returned }, contents };
  }

  if (tool === "recent_memories") {
    const memories = result?.memories;
    if (!Array.isArray(memories)) return undefined;
    memories.forEach((n, i) => collect(n, i, 0, returned, contents));
    return { record: { query: "", at: Date.now(), channel: "recent", returned }, contents };
  }

  return undefined;
}

/** Assemble the single atomic submit_batch command array for one episode.
 * `used` maps retrieval index -> the set of memory ids judged used.
 * This is the Claude Code version: goal "", tokens 0, turns from state.retrievals.length, no policy block.
 * `reason` (optional, default "") carries the SessionEnd hook's `reason` field
 * through as an additional Episode prop — additive, so pi's schema readers
 * tolerate it. */
export function buildEpisodeBatch(args) {
  const { state, outcome, failure, endedAt, used, reason = "" } = args;
  const cmds = [
    {
      op: "create_node",
      label: "Episode",
      props: {
        goal: "",
        strategy: "",
        outcome,
        started_at: state.startedAt,
        ended_at: endedAt,
        turns: state.retrievals.length,
        tokens: 0,
        confidence: 0.5,
        failure,
        reason,
      },
    },
  ];
  state.retrievals.forEach((r, i) => {
    const evRef = `#${cmds.length}`;
    cmds.push({
      op: "create_node",
      label: "RetrievalEvent",
      props: { query: r.query, at: r.at },
    });
    cmds.push({ op: "link", from: "#0", to: evRef, type: "ISSUED" });
    for (const m of r.returned) {
      cmds.push({
        op: "link",
        from: evRef,
        to: m.id,
        type: "RETURNED",
        props: { rank: m.rank, score: m.score, channel: r.channel },
      });
    }
    for (const id of used.get(i) ?? []) {
      cmds.push({ op: "link", from: evRef, to: id, type: "USED" });
    }
  });
  return cmds;
}
