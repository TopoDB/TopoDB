---
description: Seed an Obsidian-format working-memory vault from long-term memory (query or entity)
---

Seed the working-memory vault from topodb LTM using: **$ARGUMENTS**

1. The vault lives at `.topodb/vault/` under the project root. If `.topodb/` is not
   gitignored yet, append `.topodb/` to `.gitignore` first.
2. Call the `seed_vault` MCP tool with the ABSOLUTE vault path. Treat $ARGUMENTS as a
   recall query by default; if it names a single known entity, prefer
   `entity` + `hops: 2`. Never pass both.
3. Report what landed (seeded/stubs/unchanged/skipped). Treat the vault as your working
   notes for this session: read them, edit them, add new atomic notes (one fact per
   note, `[[wikilinks]]` for entities). Do NOT edit `topodb-id` lines.
4. Consolidate back to LTM later with /topodb:vault-ingest.
