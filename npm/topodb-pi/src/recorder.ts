// src/recorder.ts — pure episode-recording core: no Pi imports, no I/O.
// Everything here is deterministic and unit-tested; extension.ts supplies
// the events and ships the output to topodb-mcp submit_batch.

export interface ReturnedMemory {
  id: string;
  rank: number;
  score: number;
}

export interface RetrievalRecord {
  query: string;
  at: number; // epoch ms (semantic query time; written at episode end)
  channel: "text" | "graph";
  returned: ReturnedMemory[];
}

/** In-memory state of the currently open episode (one agent run). */
export class EpisodeBuffer {
  startedAt = 0;
  turns = 0;
  toolErrors = 0;
  retrievals: RetrievalRecord[] = [];
  private isOpen = false;

  start(nowMs: number): void {
    this.startedAt = nowMs;
    this.turns = 0;
    this.toolErrors = 0;
    this.retrievals = [];
    this.isOpen = true;
  }

  addRetrieval(r: RetrievalRecord): void {
    if (this.isOpen) this.retrievals.push(r);
  }

  bumpTurns(): void {
    if (this.isOpen) this.turns++;
  }

  noteToolError(): void {
    if (this.isOpen) this.toolErrors++;
  }

  close(): void {
    this.isOpen = false;
  }

  get open(): boolean {
    return this.isOpen;
  }
}

/** Message content -> plain text: strings pass through, block arrays
 * contribute only their `type === "text"` blocks. Defensive: anything
 * unrecognized contributes nothing. */
export function extractText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((b) =>
      b && typeof b === "object" && (b as { type?: string }).type === "text"
        ? String((b as { text?: unknown }).text ?? "")
        : "",
    )
    .join("");
}

/** Spec tokenization: lowercase alphanumeric runs of length >= 3, deduped,
 * insertion order preserved. */
export function tokenize(text: string): Set<string> {
  const out = new Set<string>();
  for (const m of text.toLowerCase().matchAll(/[a-z0-9]{3,}/g)) out.add(m[0]);
  return out;
}

/** Spec USED rule: >= 50% of the memory's tokens appear in the text. A
 * memory with no tokens is never "used". */
export function isUsed(memContent: string, text: string): boolean {
  const mem = tokenize(memContent);
  if (mem.size === 0) return false;
  const hay = tokenize(text);
  let hits = 0;
  for (const t of mem) if (hay.has(t)) hits++;
  return hits / mem.size >= 0.5;
}

/** Assemble the single atomic submit_batch command array for one episode.
 * `used` maps retrieval index -> the set of memory ids judged used. */
export function buildEpisodeBatch(args: {
  buffer: EpisodeBuffer;
  goal: string;
  outcome: "success" | "failure";
  failure: string;
  endedAt: number;
  tokens: number;
  used: Map<number, Set<string>>;
  policyVersionId?: string;
}): unknown[] {
  const { buffer } = args;
  const cmds: unknown[] = [
    {
      op: "create_node",
      label: "Episode",
      props: {
        goal: args.goal,
        strategy: "", // a reflection layer fills this later; never guess
        outcome: args.outcome,
        started_at: buffer.startedAt,
        ended_at: args.endedAt,
        turns: buffer.turns,
        tokens: args.tokens,
        confidence: 0.5,
        failure: args.failure,
      },
    },
  ];
  buffer.retrievals.forEach((r, i) => {
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
    for (const id of args.used.get(i) ?? []) {
      cmds.push({ op: "link", from: evRef, to: id, type: "USED" });
    }
  });
  if (args.policyVersionId) {
    cmds.push({
      op: "link",
      from: "#0",
      to: args.policyVersionId,
      type: "USED_POLICY",
    });
  }
  return cmds;
}
