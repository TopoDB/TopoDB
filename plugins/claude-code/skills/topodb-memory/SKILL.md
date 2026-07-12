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

Then `traverse` from a hit to gather what surrounds it. A memory's neighbours
are usually the reason it mattered.

Report only what the graph actually holds. Do not fill gaps with details from
the surrounding conversation or your own assumptions — an unstored fact
presented as recalled is worse than no recall at all.

## Store what will still be true tomorrow

Store: decisions and the reasoning behind them, constraints, a person's role or
ownership, a hard-won lesson, an architectural choice and what it rules out.

Do not store: what is already in the code or git history, what only matters to
this conversation, or anything you would have to re-verify before trusting.

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

## Names before nodes

Before `create_entity`, call `find_by_prop` on `Entity`/`name` — creating a
second "Drew" makes both halves of the graph half-right.
