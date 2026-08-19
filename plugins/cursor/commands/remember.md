---
description: Store a fact in memory (project scope, or shared if it generalizes)
---

Store this in topodb: **$ARGUMENTS**

Decide the scope first, and say which you chose and why:
- **this project** (the default) — a fact about *this* repo.
- **`shared`** (pass `scope: "shared"`) — a lesson, preference, or fact about
  the user that would be just as true in a different codebase.

Then `create_memory`, and **link it**: for each person, project, or service it
concerns, `create_entity` (find-or-create — it resolves name variants to the
existing node and reports `created: false`) and `link` the memory to it,
passing `scope: "shared"` on the link too if both ends are shared. Use the
fullest canonical name you know for each entity.

If this fact *replaces* an earlier one (a to-one relation changed — new owner,
new employer), make the entity-to-entity link with `supersede: true` so the old
edge is closed as history rather than left contradicting the new one.

If the fact includes a second name for an existing entity, `add_alias` it
instead of creating a new entity; if it defines project vocabulary, `add_synonym`
the equivalence.
