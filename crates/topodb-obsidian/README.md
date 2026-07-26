# topodb-obsidian

Deterministic Obsidian-vault ⇄ TopoDB memory transforms, shared by the CLI and MCP server.

This crate bridges a working-memory tier (Obsidian-format vault: markdown + YAML frontmatter + `[[wikilinks]]`) with TopoDB's long-term memory graph. Two deterministic, policy-free operations power the bridge:

- **Ingest** — consolidate vault notes into the graph, stamping each with a `topodb-id` for round-trip safety.
- **Seed** — materialize relevant memories as notes, so a session starts from prior knowledge.

The Obsidian *app* is optional; the format is the contract.

## The note contract

One note = one memory. Atomicity and supersession are managed at the memory level, not per-note-section.

```markdown
---
topodb-id: 01JX…          # identity key; stamped by ingest, present on seeds
kind: semantic             # optional; episodic | semantic | procedural only
type: decision             # any other scalar key → memory prop
status: open
tags: [auth, refactor]     # → `tags` prop (string list); tags are NOT entities
relates: "[[TopoDB]]"      # frontmatter values that are wikilinks → entities
---
Body is the memory content. Mentions like [[redb]] or [[ort|ONNX Runtime]]
become entities (alias-resolved or created) with memory→entity edges.
```

Reserved vault keys (never stored as props):
- `topodb-id` — identity key for round-trip safety; stamped by ingest.
- `related` — seed's entity-link list; becomes edges only.
- `entity` — stub marker for seeded entity nodes.

Mapping rules:
- **Filename** is vault-local presentation and is **not** stored in the graph. Use an explicit `title:` frontmatter key for a durable memory title; seed uses it for filenames when present.
- **Body** (frontmatter stripped, trimmed) → memory content.
- **Frontmatter `kind`** → `RememberRequest.kind` (episodic | semantic | procedural); every other scalar key → memory prop.
- **YAML sequences of scalars** flatten to one comma-joined string prop: `tags: [auth, refactor]` stores as `tags: "auth, refactor"`.
- **Wikilinks** — `[[Target]]`, `[[Target|alias]]` (entity name `Target`), `[[Target#heading]]` (entity `Target`) in body and frontmatter → entities with memory→entity edges. Embeds (`![[…]]`) are ignored.
- **Vault walk** skips `.obsidian/`, `.trash/`, any dot-directory, non-`.md` files.

## Ingest semantics

For each note in the vault:
- **No `topodb-id`** → build a `RememberRequest` and apply via `plan_remember`, inheriting content-hash dedup and entity alias resolution. The resulting memory id is stamped into the note's frontmatter.
- **Has `topodb-id`** → fetch the node. If content, kind, props, and entity links are unchanged → no-op. Otherwise, `plan_remember { supersedes: [old-id], … }` — the old memory is tombstoned as `superseded_at`, a new head memory is created, and the note's `topodb-id` is updated. The note tracks the head; the full time-course lives in the graph.
- **Fixpoint guarantee**: ingest → seed → ingest (unchanged) plans zero ops — the round-trip is idempotent when notes are unedited.

Every write is stamped with exactly one scope (global `--scope` on the CLI; per-call `scope` on MCP). Frontmatter id stamping rewrites the file atomically (temp + rename), preserving unknown frontmatter keys and body bytes exactly. See the spec (docs/superpowers/specs/2026-07-26-obsidian-integration-design.md) for the full ingest contract, including entity stubs and error handling.

Ingest is additive only: deleting a note from the vault propagates nothing back to the graph — the memory it was seeded from lives on as-is. Retiring a memory (soft-tombstoning it as `forgotten_at`) is the `forget` verb's job, invoked separately; ingest never infers a delete from a missing file.

Embeddings are applied only when the caller supplies an embed hook: the MCP server's `ingest_vault`/`seed_vault` always do (via its configured embedder), while the CLI's `obsidian-ingest`/`obsidian-seed` are text+graph only (no `embed` hook), so vector recall over CLI-ingested memories is unavailable until something else embeds them.

## Seed semantics

Two selectors, both existing reads filtering liveness via the engine's tombstone set:

- **Query** — `--query "auth refactor" --k 12` uses `Db::recall` (RRF-fused BM25 + vector + graph; CLI has text+graph only). Server embeddings come from the MCP server's embedder.
- **Entity neighborhood** — `--entity topodb --hops 2` scoped k-hop traversal from the anchor entity.

Output: one note per selected memory, frontmatter = `topodb-id`, `kind`, and props (tombstones/reserved props never serialized). Body = content exactly (unchanged notes are no-op on re-ingest; links NOT appended to body). Memory→entity edges render as a `related:` frontmatter list of `"[[Entity]]"` strings. Filenames derive from the `title` prop when present, else the first line of content (slugified); collisions get a short id suffix.

Entity stubs (minimal notes: `topodb-id`, `entity: true`, aliases if any; empty body) are seeded so wikilinks resolve in Obsidian. Non-clobbering: seed **skips** existing files with differing content (reported as skipped); `--overwrite` forces. Files that match byte-for-byte are silent no-ops. See the spec for the full seed contract.

## CLI & MCP surfaces

- **CLI** (`topodb-cli`): `obsidian-ingest <vault-dir> [--scope S] [--dry-run]` and `obsidian-seed <vault-dir> (--query Q [--k N] | --entity E [--hops H]) [--overwrite]`.
- **MCP** (`topodb-mcp`): `ingest_vault { vault, scope?, dry_run? }` and `seed_vault { vault, query?, k?, entity?, hops?, overwrite? }`.

Both surfaces return compact JSON report with counts (ingested/superseded/deduplicated/skipped for ingest; seeded/stubs/unchanged/skipped for seed) and per-file errors.

---

For the full design, principles (engine, not policy), and testing notes, see [docs/superpowers/specs/2026-07-26-obsidian-integration-design.md](../../docs/superpowers/specs/2026-07-26-obsidian-integration-design.md).
