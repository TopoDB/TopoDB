---
description: Search memory for what we know about something
---

Search topodb for: **$ARGUMENTS**

Call `search_memories` with that query. Word forms and typos are handled
(stemming plus fuzzy fallback), but synonyms are not — if it comes back empty,
retry with different words, and try the name of the person/project itself;
entity names are indexed too. If there
are hits, `traverse` from the most relevant one (max_hops 2) to pull in the
surrounding context — and `get_edges` when you need a node's current relations
(`open_only: false` shows superseded history). Then summarize what is actually
known — entities, decisions, and when they were recorded. If there are still
no hits, say so plainly rather than reconstructing an answer from the current
conversation.
