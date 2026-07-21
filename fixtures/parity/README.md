# Parity Fixtures

Shared fixture format for parity tests across TopoDB bindings (Python, JavaScript, CLI).

## Fixture Format

Each `.json` file defines:

### Top-level keys

- `now_ms` (number): Timestamp in milliseconds for the primary batch submission. Pinned for determinism.
- `index_spec` (object): Index specification passed to `TopoDB.open_with()`.
  - `equality`: Array of index definitions for exact/normalized property lookups.
  - `text`: Array of text index definitions for BM25 search.
- `batch` (array): Batch of DSL commands to submit at `now_ms`.
- `batch2` (array, optional): Second batch submitted at `now_ms2`. Used for temporal operations (e.g., closing edges).
- `now_ms2` (number, optional): Timestamp for the second batch. Required if `batch2` is present.
- `checks` (array): Array of assertions to verify after submission(s).

### "#N" Back-References

Any string value matching `"#N"` (where N is a digit) in `batch`, `batch2`, `args`, or `expect_*` keys is a back-reference to the id at index N in the ids returned by the first `submit()` call.

Example:
- `submit(batch)` returns `{"ids": ["id1", "id2", "id3"], ...}`
- `"#0"` resolves to `"id1"`
- `"#1"` resolves to `"id2"`
- `"#2"` resolves to `"id3"`

When `batch2` is present, `"#N"` in `batch2` still refers to ids from `batch`.

### Checks Format

Each check has:
- `call` (string): Method name — one of `"node"`, `"nodes_by_label"`, `"search_text"`, `"traverse"`.
- `args` (object): Arguments to pass to the method. Supports `"#N"` back-references.
- `expect_*` (varies by call):
  - `node`: `expect_label` (string)
  - `nodes_by_label`: `expect_ids` (array of id references, order-sensitive)
  - `search_text`: `expect_ids` (array of id references, order-sensitive by score)
  - `traverse`: `expect_node_ids` (array of id references), `expect_edge_count` (number, optional)

### Batch DSL

Commands are plain objects with an `"op"` field. Common commands:
- `{"op": "create_entity", "name": "..."}` → label `"Entity"`, prop `"name"`
- `{"op": "create_memory", "content": "..."}` → label `"Memory"`, prop `"content"`
- `{"op": "create_node", "label": "...", "props": {...}}` → custom node
- `{"op": "link", "from": "#N", "to": "#M", "type": "..."}` → edge (type stored lowercase)
- `{"op": "close_edge", "id": "#N"}` → close an edge (used in `batch2` for temporal tests)
- `{"op": "set_node_props", "id": "#N", "props": {...}}` → update properties

### Important Details

- **Label Casing**: Canonical labels are capitalized: `"Entity"`, `"Memory"`. Custom labels are case-sensitive.
- **Edge Type Normalization**: Edge types are stored in lowercase. `{"type": "ABOUT"}` is stored/returned as `"about"`.
- **Text Index Requirement**: Any fixture with `search_text` checks must include a text index in `index_spec`.
- **Temporal Semantics**: Edges have `valid_from` (set at submit time) and `valid_to` (from `close_edge`). The `traverse` check's `as_of` parameter (in milliseconds) filters edges by their validity window.

## Examples

### basic-graph.json
Entity + Memory + link, demonstrating basic node retrieval and traversal.

### search-and-recall.json
Two memories with text index, demonstrating BM25 search ranking.

### temporal-close.json
Two entities with a link, followed by closing the edge in a second batch. Demonstrates temporal visibility: the edge is visible before the close time and gone after.
