# topodb-okf

Deterministic Open Knowledge Format (OKF v0.2) bundle ⇄ TopoDB memory transforms, shared by the CLI and MCP server.

This crate bridges an OKF bundle — a directory of markdown concept pages with nested-YAML frontmatter, `[label](/path.md)` links, and reserved `index.md`/`log.md` files — with TopoDB's long-term memory graph. Like [`topodb-obsidian`](../topodb-obsidian), the bridge is two policy-free, LLM-free operations:

- **Ingest** — consolidate an OKF bundle into the graph.
- **Seed** — materialize relevant memories back out as an OKF bundle.

`seed → ingest → seed` and `ingest → seed → ingest` are no-ops on unchanged input: every promotion is reversible.

## The page contract

A concept page = an **Entity** (carrying a bundle-relative `path` prop — the identity anchor) plus one attached `about` **Memory** (the page body). High-value provenance is promoted to first-class nodes and edges; the long tail is flattened to dotted-key props.

- **`generated` / `verified`** (`by` / `at`) → actor entities with `generated_by` / `verified_by` edges (the `at` timestamp rides the edge).
- **`sources`** → source entities with `sourced_from` edges; `author` → `authored_by`.
- **Body links** `[label](/path.md)` → `references` edges, resolved against the equality-indexed `path` prop.
- **`type`** → an open-vocabulary entity prop (never validated); `tags`/`title` map as expected.
- **`topodb-id`** — our OKF extension stamp for round-trip identity; absent on a foreign bundle's first ingest, stamped thereafter and on every seeded page.

## Index requirement

Ingest and seed both need `okf_spec()` — the JSON default index spec plus an equality index on `(Entity, path)` so path dedup and body-link resolution work.

## Surfaces

Exposed through the same CLI (`topodb-cli`) and MCP (`topodb-mcp`) surfaces as the other ingest layers, returning a compact JSON report with per-file errors.

---

For the full design, see the OKF integration spec under `docs/superpowers/specs/`.
