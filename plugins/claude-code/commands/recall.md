---
description: Search memory for what we know about something
---

Search topodb for: **$ARGUMENTS**

Call `search_memories` with that query. Word forms and typos are handled
(stemming plus fuzzy fallback), and learned synonyms (`add_synonym`) expand
automatically — but only equivalences someone has taught it; if it comes back
empty, retry with different words, and try the name of the person/project
itself; entity names are indexed too. Linked context now arrives
automatically (graph boost pulls in a hit's immediate neighbours), so
`traverse` from the most relevant hit (max_hops 2) mainly for going deeper
than that — and `get_edges` when you need a node's current relations
(`open_only: false` shows superseded history). Then summarize what is actually
known — entities, decisions, and when they were recorded. If there are still
no hits, say so plainly rather than reconstructing an answer from the current
conversation.
