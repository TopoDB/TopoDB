# TopoDB on-disk format (v2)

This is the durable contract for a TopoDB redb file. Code wins if it differs from
this document. The v1 fixture (`crates/topodb/tests/fixtures/v1.redb`) is frozen as
migration input; the v2 fixture is exercised by `format_fixture.rs`.

## Version and migration

`storage::FORMAT_VERSION` is **2**. `Storage::open_with` creates new files at v2,
opens v2 files directly, rejects a version greater than 2 with
`TopoError::UnsupportedFormat`, and migrates v1 in one redb write transaction using
`migrate::migrate_v1_to_v2`. A crash during migration leaves the original v1
transaction intact. Migration rewrites nodes and edges, externalizes embeddings and
constructs the dictionary; it never rewrites the self-describing op log, postings,
FTS tables, counters, or unrelated metadata. Redb free space means physical file size
need not shrink even where logical row bytes do.

## Tables

`storage.rs` defines ten tables:

| table | key | value |
|---|---|---|
| `ops` | u64 sequence | raw postcard `Op` (never interned or framed) |
| `meta` | UTF-8 string | key-specific bytes |
| `nodes` | 16-byte BE node ULID | framed postcard `disk::NodeRecordDisk` |
| `edges` | 16-byte BE edge ULID | framed postcard `disk::EdgeRecordDisk` |
| `embeddings` | 16-byte BE node ULID | framed postcard `(String, Vec<f32>)` |
| `dict` | `[kind:u8][id:u32 BE]` | UTF-8 interned string |
| `postings` | `scope_key ++ term` | postcard postings |
| `fts_docs` | node key | postcard token count |
| `fts_stats` | scope key | postcard `(doc_count, total_len)` |
| `counters` | node key | postcard `AccessStats` |

`node_key`, `edge_key`, and `scope_key` in `storage.rs` define the fixed-width key
encodings. `dict::DICT` has append-only namespaces: `0x00` label, `0x01` edge type,
and `0x02` property key. Dictionary rows are derived, append-only state; unknown IDs
while decoding are corruption and return `TopoError::Encoding`.

## Records and value frame

Public in-memory `NodeRecord` and `EdgeRecord` remain string-carrying. Their disk twins
in `disk.rs` replace labels/types/property keys with dictionary u32 IDs. Node disk rows
have no embedding: `Storage::read_node` joins the cold `embeddings` row back before
returning a public record. This avoids rewriting a vector for ordinary property edits.

Every `nodes`, `edges`, and `embeddings` value has the append-only frame registry from
`codec.rs`: `0x00` raw postcard, `0x01` reserved (zstd), `0x02` lz4 block with prepended
uncompressed size. Values under 512 bytes are raw. Larger values retain lz4 only when
it saves at least 10%; decode rejects a declared result above 256 MiB. `lz4_flex` is
pure Rust; no C zstd dependency is used.

## Evolution policy

On-disk serde enum variants and codec/dictionary kind registries are append-only. The
op log deliberately retains full strings and no frame so replay remains independent of
dictionary assignment. `Storage::rebuild_state_from_ops` drains and regenerates derived
v2 rows and dictionary state while replaying this self-describing log.
