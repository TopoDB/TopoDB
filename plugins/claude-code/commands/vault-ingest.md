---
description: Consolidate the Obsidian working-memory vault into long-term memory
---

Consolidate the working-memory vault into topodb LTM. **$ARGUMENTS**

1. Call the `ingest_vault` MCP tool with the ABSOLUTE path of `.topodb/vault/` under
   the project root (pass `dry_run: true` first if $ARGUMENTS asks for a preview).
2. Ingest stamps `topodb-id` into new notes and supersedes memories whose notes you
   edited — the graph keeps full history, so this is safe to run repeatedly.
3. Report the counts (ingested/superseded/deduplicated/skipped) and surface any
   per-file errors verbatim so the user can fix the notes.
