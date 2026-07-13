---
description: Store a fact in memory (project scope, or shared if it generalizes)
---

Store this in topodb: **$ARGUMENTS**

Decide the scope first, and say which you chose and why:
- **this project** (the default) — a fact about *this* repo.
- **`shared`** (pass `scope: "shared"`) — a lesson, preference, or fact about
  the user that would be just as true in a different codebase.

Then `create_memory`. If it concerns a person, project, or service, check
`find_by_prop` for that entity, create it if it is genuinely new, and `link` the
memory to it — passing `scope: "shared"` on the link too if both ends are shared.
