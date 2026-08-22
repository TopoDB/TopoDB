# topodb

Embedded Node.js bindings for [TopoDB](https://github.com/TopoDB/TopoDB), an
agent-memory engine: a temporal property graph with scoped recall, stored in a
single file. The engine runs in-process via a native (napi) addon — no server,
no daemon.

## Install

```bash
npm i topodb
```

Prebuilt binaries are pulled in automatically via platform packages (see
[Platform support](#platform-support)). Node.js >= 18.

## Quickstart

```js
const { TopoDB, ops } = require('topodb')

// Indexing is opt-in per (label, prop): declare what you'll look up by
// equality and what full-text search should cover. Plain TopoDB.open()
// indexes nothing (fine for pure graph workloads); TopoDB.openStored()
// reopens a file with the spec it was created with.
const db = await TopoDB.openWith('memory.redb', {
  equality: [{ label: 'Entity', prop: 'name' }],
  text: [{ label: 'Memory', prop: 'content' }],
})

// Write: build a batch with the ops builders, then submit it.
// '#N' back-references the id created by the Nth command in the same batch.
// A scope is "shared" or a ULID; with no default scope passed here,
// everything lands in "shared".
const { ids } = await db.submit([
  ops.createEntity('ada'),
  ops.createMemory('ada wrote the first program'),
  ops.link('#1', '#0', 'ABOUT'),
])

// Read: every read takes an array of scopes to search across.
const hits = await db.recall(['shared'], 'first program', 5)
for (const { node, score } of hits) {
  console.log(score, node.label, node.props)
}

// Walk the graph outward from a seed node.
const subgraph = await db.traverse(['shared'], [ids[0]], 2)
console.log(subgraph.nodes.length, subgraph.edges.length)

db.close() // TopoDB also implements Symbol.dispose (`using db = ...`)
```

The ops builders (`ops.createEntity`, `ops.createMemory`, `ops.createNode`,
`ops.link`, `ops.setNodeProps`, `ops.removeNode`, `ops.closeEdge`,
`ops.setEmbedding`) produce plain objects in the batch-submit DSL shared with
the TopoDB CLI and MCP server — you can also build the objects by hand.

Beyond `recall` and `traverse`, the read surface includes `node`,
`nodesByLabel`, `nodesByProp`, `nodesByFloatRange`, `edgesFrom`, `searchText`,
`searchVector`, and `suggestLinks`; `subscribe` yields a change feed you can
`for await` over. See `index.d.ts` for the full typed API.

## The multi-scope read model

Scopes partition one database file into independent memory spaces (for
example, one per project, plus a shared space).

- **Every read takes `scopes: string[]`** and returns results from the union
  of those scopes. Reading with `['project-a', 'shared']` sees both spaces.
- **Every write is stamped with exactly one scope.** Each command in a
  `submit` batch can carry its own `scope`; commands that don't are stamped
  with the batch's `defaultScope` argument.

The read set can be wider than the write scope — the common agent pattern is
"read project + shared, write project".

## Errors

Engine errors are rejected as ordinary `Error`s decorated with a `code`:

| `err.code` | Meaning | Extra fields |
| --- | --- | --- |
| `STORAGE` | Underlying storage failure (I/O, corrupt file, lock) | |
| `ENCODING` | A value could not be encoded/decoded | |
| `REJECTED` | The batch was invalid; nothing was applied | |
| `COMPACTED` | The requested op-log range was compacted away | `err.oldest` — oldest retained seq |
| `CLOSED` | The handle was used after `close()` | |
| `UNSUPPORTED_FORMAT` | The file's format version is newer than this build reads | `err.found`, `err.supported` |

```js
try {
  await db.opsSince(0)
} catch (err) {
  if (err.code === 'COMPACTED') console.log('resume from', err.oldest)
}
```

## Bi-temporal edges

Every edge carries two independent time axes:

- **World time** — `valid_from` / `valid_to` (ms): when the relationship was
  true in the world. Caller-settable via `ops.link({ validFrom })` and
  `ops.closeEdge(id, validTo)`; `valid_to: null` means still valid.
- **Belief time** — `recorded_at` / `superseded_at` (ms): when the database
  believed it. Engine-stamped, never caller-settable; `superseded_at: null`
  means currently believed.

The two axes differ whenever a fact is recorded late or corrected after the
fact, which is what lets reads answer both "what was true then" and "what did
we believe then" (e.g. `traverse` with `asOf`).

## Platform support

Prebuilt `.node` binaries ship as scoped platform packages
(`@topodb/topodb-<platform>`, e.g. `@topodb/topodb-linux-x64-gnu`) listed as
`optionalDependencies` — npm installs
only the one matching your machine. The main `topodb` package contains no
binaries.

| OS | x64 | arm64 |
| --- | --- | --- |
| Linux | ✅ | ✅ |
| macOS | ✅ | ✅ |
| Windows | ✅ | — |

On other platforms, build from source with a Rust toolchain:
`npm run build` inside `crates/topodb-node`.

## Embedded vs. MCP

This package **embeds** the engine: your process opens the database file
directly, which is the right fit for building your own memory-backed tools and
services. If you want TopoDB as memory for an **agent client** (Claude Code,
Cursor, Codex CLI, Zed, …), you usually don't want bindings at all — run the
`topodb-mcp` server and let the client speak MCP to it. See
[`docs/agent-clients.md`](../../docs/agent-clients.md) for per-client setup
and how automatic the memory experience can be in each.

Python bindings with the same wire shapes are published as
[`topodb` on PyPI](https://pypi.org/project/topodb/).

## License

See the repository root for license details.
