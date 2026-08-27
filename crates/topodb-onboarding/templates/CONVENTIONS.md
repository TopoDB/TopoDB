version: 2

## Scope Discipline

Reads filter by a SET of scopes; a write stamps EXACTLY ONE scope. Use project-specific scopes (e.g. `auth`, `schema`) and the shared `shared` scope for cross-project facts. When remembering, always specify the appropriate scope — it gates recall and prevents overload.

## Writing Style

Store one fact per memory. Use `remember()` as your atomic call — it finds or creates entities, links them, and stores the memory in one step. When a fact changes, supersede the old memory rather than creating a duplicate. Keep memories concise and actionable.

## When to Remember

Record decisions, design lessons, and non-obvious facts that future you or teammates need to know. Do NOT duplicate what Git and code already record — don't memorize commit hashes or file content. Focus on context: why a choice was made, trade-offs considered, blockers resolved, or patterns the repo depends on.

## When to Merge

When two memories are the same fact reworded, merge them with `consolidate_memories`. You pick which copy to keep. Do not merge from similarity alone — contradictions score high too.

## When to Retire

When a fact is replaced, pass `supersedes` on `remember`. When a memory should never surface again, `forget` it. `lifecycle_candidates` ranks cold memories and proposes; you act.

## Hygiene

`memory_health` at session start reports duplicate pairs, supersessions, orphans, and stale memories. Drill into non-zero counts with the `find_*` scans. Scans never delete.

## Conflicts

When `remember` returns `supersession_candidates`, supersede the stale side, consolidate a duplicate, or ignore a false alarm. Uncertainty stays in the graph until something is judged.

## Recall Discipline

Search first with your best vocabulary. If the search doesn't find what you expected, retry with different terms — synonyms, broader/narrower concepts. Only conclude nothing is stored after several attempts. Use traverse to explore related facts from your best hit. Build mental maps: link related memories to make them easier to find next time.
