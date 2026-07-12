---
description: Search memory for what we know about something
---

Search topodb for: **$ARGUMENTS**

Call `search_memories` with that query. If there are hits, `traverse` from the
most relevant one (max_hops 2) to pull in the surrounding context, then
summarize what is actually known — entities, decisions, and when they were
recorded. If there are no hits, say so plainly rather than reconstructing an
answer from the current conversation.
