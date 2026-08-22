# topodb

Embedded, temporal, graph-native memory for AI agents — Python bindings for the
[TopoDB](https://github.com/TopoDB/TopoDB) engine.

TopoDB is a single-file property graph (redb-backed) with scoped recall, text +
vector search, and bi-temporal edges. These bindings embed the engine in your
process: no server, no daemon, one `.redb` file on disk.

## Install

```
pip install topodb
```

Prebuilt wheels (abi3, Python ≥ 3.9):

| Platform | Architecture |
| --- | --- |
| Linux (manylinux) | x86_64 |
| Linux (manylinux) | aarch64 |
| macOS | universal2 (x86_64 + arm64) |
| Windows | x64 |

Other platforms build from the sdist (requires a Rust toolchain via maturin).

## Quickstart

```python
import topodb
from topodb import ops

# Indexing is opt-in per (label, prop): declare equality lookups and what
# full-text search should cover. Plain TopoDB.open() indexes nothing (fine
# for pure graph workloads); TopoDB.open_stored() reopens a file with the
# spec it was created with.
spec = {
    "equality": [{"label": "Entity", "prop": "name"}],
    "text": [{"label": "Memory", "prop": "content"}],
}

with topodb.TopoDB.open_with("memory.redb", spec) as db:
    r = db.submit([
        ops.create_entity("ada"),
        ops.create_memory("ada wrote the first program"),
        ops.link("#1", "#0", "ABOUT"),   # "#n" back-references the n-th op's new id
    ])
    ada_id, memory_id, edge_id = r["ids"]

    scopes = ["shared"]                  # reads always name the scopes they may see
    hits = db.search_text(scopes, "first program", 5)
    print(hits[0]["node"]["props"]["content"], hits[0]["score"])

    sg = db.traverse(scopes, seeds=[memory_id], max_hops=2)
    print([n["id"] for n in sg["nodes"]], len(sg["edges"]))
```

`TopoDB.open(path)` creates or opens a database with the default index spec;
`TopoDB.open_with(path, spec)` sets an explicit index spec (equality + text
indexes) on create; `TopoDB.open_stored(path)` reopens with whatever spec the
file already carries. The handle is a context manager — leaving the `with`
block closes it, and any later call raises `ClosedError`.

## Writes: `ops` builders + `submit`

All mutation goes through `db.submit(batch)`, one atomic batch of command
dicts. The `topodb.ops` module builds those dicts (the same wire shapes the
CLI and MCP server speak):

```python
ops.create_entity(name, scope=None)
ops.create_memory(content, scope=None)
ops.create_node(label, props=None, scope=None)
ops.link(from_, to, type, props=None, scope=None, valid_from=None)
ops.set_node_props(id, props)      # a None value inside props deletes that prop
ops.remove_node(id)
ops.close_edge(id, valid_to=None)
ops.set_embedding(id, model, vector)
```

Within a batch, `"#0"`, `"#1"`, … refer to the ids created by earlier ops in
the same batch. `submit` returns `{"first_seq", "last_seq", "ids"}` — one ULID
string per op. Pass `now_ms=` to pin the write timestamp (defaults to the wall
clock) and `default_scope=` to stamp the whole batch (see below).

## The multi-scope read model

Every read takes a **list of scopes** as its first argument and only sees data
stamped with one of them. Every write is stamped with exactly **one** scope —
per-op via the builder's `scope=` parameter, or batch-wide via
`db.submit(batch, default_scope=...)`. A scope is `"shared"` or a ULID string.

This asymmetry is the point: an agent can read across `["shared", project_scope]`
while writing only into `project_scope`.

```python
db.node(scopes, id)                                  # dict or None
db.nodes_by_label(scopes, label)
db.nodes_by_label_newest(scopes, label, k)
db.nodes_by_prop(scopes, label, prop, value)         # equality-indexed props only
db.nodes_by_prop_normalized(scopes, label, prop, value)
db.nodes_by_float_range(scopes, prop, lo, hi)
db.edges_from(scopes, id, type=None)
db.traverse(scopes, seeds=[...], max_hops=n,
            edge_types=None, direction="both", as_of=None)
db.search_text(scopes, query, k)
db.search_vector(scopes, model, vector, k)
db.recall(scopes, query, k, vector=(model, vec), labels=None, now_ms=None)
db.suggest_links(scopes, id, k, model=None)
```

Nodes come back as plain dicts: `{"id", "scope", "label", "props"}`. Search
hits are `{"node", "score"}`.

## Bi-temporal edges

Edges carry two independent time axes, both in the wire dict:

- **World time** — `valid_from` / `valid_to`: when the fact was true in the
  world. Settable on write (`ops.link(..., valid_from=...)`,
  `ops.close_edge(id, valid_to=...)`); `valid_to` is `None` while the edge is
  open. `traverse(..., as_of=t)` answers "what was true at *t*".
- **Belief time** — `recorded_at` / `superseded_at`: when the database learned
  and stopped believing the fact. Stamped by the engine, never settable;
  `superseded_at` is `None` while the edge is current.

The two differ whenever a fact is recorded late or corrected after the fact.
Full edge shape:

```python
{"id", "scope", "type", "from", "to", "props",
 "valid_from", "valid_to", "recorded_at", "superseded_at"}
```

## Errors

All errors derive from `topodb.TopoDBError`, so one `except` catches
everything; subclasses carry the detail:

| Exception | Meaning | Extra attributes |
| --- | --- | --- |
| `StorageError` | I/O or storage-layer failure | |
| `EncodingError` | corrupt or undecodable stored data | |
| `RejectedError` | invalid batch, arguments, or unindexed-prop query | |
| `CompactedError` | requested ops feed range was compacted away | `oldest` — first seq still available |
| `BusyError` | database file held by another process; retryable | |
| `ClosedError` | handle used after `close()` | |
| `UnsupportedFormatError` | file format version mismatch | `found`, `supported` |

## Change feed

`db.subscribe(capacity)` returns a `Subscription` that yields committed ops as
dicts. `sub.next(timeout=...)` returns the next event or `None` on timeout
(the GIL is released while waiting), and the object is also a plain iterator
that ends when the database closes. `db.ops_since(seq)` / `db.current_seq()` /
`db.compact_ops(seq)` cover catch-up and log compaction.

## Embedded vs MCP

These bindings are the **embedded** client: in-process, zero-IPC, one Python
process owning the file. If you want TopoDB behind an agent framework instead
— shared across sessions, spoken over the Model Context Protocol — use the
`topodb-mcp` server. Both speak the same wire shapes and the same batch DSL;
see [`docs/agent-clients.md`](https://github.com/TopoDB/TopoDB/blob/main/docs/agent-clients.md)
for the trade-offs.

## Versioning

0.1.0 is the first published release of these bindings; it wraps the frozen
0.1 engine API. See the repository `CHANGELOG.md` for history from here on.
