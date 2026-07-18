---
name: topodb-memory
description: Use when the user refers to past work, decisions, or people ("what did we decide", "who owns X", "last time"), and when a session produces a fact, decision, or lesson worth keeping. Reads span this project plus shared knowledge; writes default to this project.
---

# topodb memory

Every session has a memory: a graph of memories and entities, spanning **this
project** plus a **`shared`** layer that crosses projects.

## Recall before you guess

When the user refers to earlier work — "what did we decide about X", "who owns
Y", "last time we tried this" — call `search_memories` **before** answering from
the conversation alone. Reads already span this project and `shared`; you do not
pass a scope to read.

Search matches **exact tokens** (lowercased, no stemming): "databases" does not
match "database". If a query comes back empty or thin, retry with other word
forms or synonyms and raise `k` before concluding nothing is stored. Entity
names are searchable too, so a person or project name is a good query. Results
are recency-weighted — fresher memories outrank stale ones at equal relevance.

Then `traverse` from a hit to gather what surrounds it. A memory's neighbours
are usually the reason it mattered. `get_edges` on a node shows its current
relations (and their history, with `open_only: false`).

Report only what the graph actually holds. Do not fill gaps with details from
the surrounding conversation or your own assumptions — an unstored fact
presented as recalled is worse than no recall at all.

If the memory tools are not available in this session, say so plainly and
continue without them — do not pretend the graph is empty or invent recalled facts.

## Store what will still be true tomorrow

Store: decisions and the reasoning behind them, constraints, a person's role or
ownership, a hard-won lesson, an architectural choice and what it rules out.

Do not store: what is already in the code or git history, what only matters to
this conversation, or anything you would have to re-verify before trusting.

**Always link what you store.** After `create_memory`, `create_entity` the
people/projects/services it concerns and `link` the memory to them (e.g.
`about`) — an unlinked memory can only ever be found by keyword search, never
by traversing from the things it is about. Both writes are safe to repeat:
`create_entity` is find-or-create (a re-typed name resolves to the existing
node), and `link` reuses an existing open edge instead of duplicating it.
Reuse edge-type names the graph already has (`works_at`, not also
`employed_by`).

## When a fact changes

Facts supersede, they don't overwrite. When a to-one relation changes — new
employer, new owner, moved teams — `link` the new edge with `supersede: true`:
it atomically closes the other open same-type edges from that node, keeping the
old fact as history. For anything else that stops being true, find the edge
with `get_edges` and `close_edge` it. Then store a memory recording *why* it
changed, if you know.

## Project or shared — the one choice that matters

Writes land in **this project's scope** by default. That is right for most
facts.

Pass `scope: "shared"` explicitly when a lesson **generalizes beyond this repo**
— a fact about the user, a preference in how they want to work, a hard-won
lesson that would be just as true in a different codebase.

`shared` is also the right scope for an entity that exists across projects (a
person, a service). When you `link` a shared memory to a shared entity, **pass
`scope: "shared"` on the `link` too.** An edge takes the project scope unless
you say otherwise, which would leave the two shared nodes connected only from
inside this repo, and disconnected from every other one.

## One name per thing

`create_entity` is find-or-create: it matches names case- and whitespace-
insensitively across this project, `shared`, and your read scopes, and returns
the existing node (`created: false`) instead of minting a twin. What it cannot
catch is a genuinely different name for the same thing — "Drew" vs "Drew
Powell". Use the fullest canonical name you know, and keep using it: a second
name makes both halves of the graph half-right.
