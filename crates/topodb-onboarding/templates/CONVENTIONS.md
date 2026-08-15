version: 1

## Scope Discipline

Reads filter by a SET of scopes; a write stamps EXACTLY ONE scope. Use project-specific scopes (e.g. `auth`, `schema`) and the shared `shared` scope for cross-project facts. When remembering, always specify the appropriate scope — it gates recall and prevents overload.

## Writing Style

Store one fact per memory. Use `remember()` as your atomic call — it finds or creates entities, links them, and stores the memory in one step. When a fact changes, supersede the old memory rather than creating a duplicate. Keep memories concise and actionable.

## When to Remember

Record decisions, design lessons, and non-obvious facts that future you or teammates need to know. Do NOT duplicate what Git and code already record — don't memorize commit hashes or file content. Focus on context: why a choice was made, trade-offs considered, blockers resolved, or patterns the repo depends on.

## Recall Discipline

Search first with your best vocabulary. If the search doesn't find what you expected, retry with different terms — synonyms, broader/narrower concepts. Only conclude nothing is stored after several attempts. Use traverse to explore related facts from your best hit. Build mental maps: link related memories to make them easier to find next time.
