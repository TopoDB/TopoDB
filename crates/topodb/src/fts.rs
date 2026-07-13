//! Full-text search: tokenization, per-node declared text extraction,
//! transactional per-scope postings maintenance, and scoped BM25 ranking.
//!
//! The postings/doc-length/per-scope-corpus tables are maintained *inside the
//! same redb write transaction* as the graph state, via [`fts_update`] — so a
//! batch is atomic across NODES/EDGES **and** the text index, and a rejected
//! batch leaves the index untouched. `apply_batch` collects
//! `(scope_id, slot, old_text, new_text)` during its op loop and drives
//! `fts_update` after every op succeeds; `rebuild_state_from_ops` reuses the
//! very same function during replay (after draining POSTINGS/FTS_DOCS/
//! FTS_STATS).
//!
//! FTS_DOCS/FTS_STATS layout (unchanged since v3, re-keyed from the ULID/
//! `Scope`-keyed v2 layout by W2b): FTS_DOCS is keyed by the node's 8-byte BE
//! dense slot (matching NODES/EDGES/EMBEDDINGS/COUNTERS); FTS_STATS is keyed
//! by the [`crate::scopes::ScopeRegistry`]-interned 4-byte `scope_id`, so a
//! document in one scope never shifts another scope's df/avgdl. Both hold
//! plain-postcard payloads.
//!
//! POSTINGS layout (v4, chunked — Task 6 of the storage-format-v4 plan;
//! `FORMAT_VERSION` still reads 3 until Task 7 flips it, so this is a
//! mid-branch on-disk state no released build can read): a term's postings
//! are split across one or more chunk keys `scope_id.to_be_bytes() ++
//! term-UTF-8 ++ chunk.to_be_bytes()` (`chunked_posting_key`) rather than one
//! unbounded row — a single hot term's postings can no longer grow into one
//! value that must be fully rewritten on every touch, which is what made
//! incremental maintenance quadratic at scale (see BENCHMARKS.md's v3
//! escalation finding). Each chunk's value is a `POSTINGS_BLOCK_FORMAT_V0`
//! framed block of delta-varint `(slot_delta, tf)` pairs, ascending by slot
//! (`encode_posting_block`/`decode_posting_block`), wrapped in the same
//! `codec::frame_value` framing as other values. `set_posting` maintains
//! this via one bounded prefix range scan for the term's chunk keys, then
//! either a last-chunk append (new highest-slot doc; split at
//! `POSTINGS_CHUNK_TARGET` if needed) or a covering-chunk decode/mutate/
//! rewrite (update, remove, or an out-of-order new slot) — see its doc
//! comment. `read_posting` decodes and concatenates every chunk (used by
//! scoring, which needs every entry regardless); `posting_df` sums each
//! chunk's `posting_block_count` header without decoding entries — the df
//! fast path `search_text` uses before deciding whether df is even nonzero.
//!
//! `search_text` reads the committed tables through a fresh read
//! transaction, scores BM25 within each requested scope's own corpus, then
//! resolves the winning slots straight to `NodeRecord`s from the SAME
//! transaction's NODES table — no separate snapshot hop, no ULID
//! indirection.

use crate::adj::{read_varint, write_varint};
use crate::codec::{frame_value, unframe_value};
use crate::db::Db;
use crate::error::{storage_err, TopoError};
use crate::ids::ScopeSet;
use crate::index::IndexSpec;
use crate::props::PropValue;
use crate::state::NodeRecord;
use crate::storage::{read_node_by_slot, slot_key, FTS_DOCS, FTS_STATS, NODES, POSTINGS};
use crate::vector_store::{EMBEDDING_REF, VECTORS};
use redb::{ReadableTable, Table};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// BM25 term-frequency saturation.
pub(crate) const K1: f32 = 1.2;
/// BM25 length-normalisation strength.
pub(crate) const B: f32 = 0.75;

/// Lowercase, split on every non-alphanumeric boundary, drop empty tokens.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The declared text of a node under `spec.text`: every `Str` prop whose
/// `(label, prop)` is declared for this node's label and present in its props,
/// joined with a single space in `spec.text` **declaration order** (the order
/// is load-bearing — it fixes the document's token sequence and thus its
/// length). `None` if the node's label has no declared text props, or none of
/// them are present as `Str` values.
pub(crate) fn doc_text(spec: &IndexSpec, rec: &NodeRecord) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for pi in &spec.text {
        if pi.label != rec.label {
            continue;
        }
        if let Some(PropValue::Str(s)) = rec.props.get(&pi.prop) {
            parts.push(s.as_str());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Per-term frequencies for a token list.
fn term_freqs(tokens: &[String]) -> BTreeMap<&str, u32> {
    let mut m: BTreeMap<&str, u32> = BTreeMap::new();
    for t in tokens {
        *m.entry(t.as_str()).or_insert(0) += 1;
    }
    m
}

/// Reads a scope's `(doc_count, total_len)` corpus stats from FTS_STATS
/// (`(0, 0)` if absent).
fn read_stats(
    stats: &impl ReadableTable<&'static [u8], &'static [u8]>,
    scope_id: u32,
) -> Result<(u64, u64), TopoError> {
    let key = scope_id.to_be_bytes();
    match stats.get(key.as_slice()).map_err(storage_err)? {
        Some(v) => postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string())),
        None => Ok((0, 0)),
    }
}

/// Writes a scope's `(doc_count, total_len)` corpus stats to FTS_STATS. When
/// the scope's last document is removed (`doc_count == 0`) the row is dropped
/// entirely, so an emptied scope leaves no stale row claiming documents (same
/// empty-key doctrine as the postings and prop index).
fn write_stats(
    stats: &mut Table<'_, &'static [u8], &'static [u8]>,
    scope_id: u32,
    doc_count: u64,
    total_len: u64,
) -> Result<(), TopoError> {
    let key = scope_id.to_be_bytes();
    if doc_count == 0 {
        stats.remove(key.as_slice()).map_err(storage_err)?;
    } else {
        let bytes = postcard::to_allocvec(&(doc_count, total_len))
            .map_err(|e| TopoError::Encoding(e.to_string()))?;
        stats
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}

/// The doc length (in tokens) recorded for `slot` in FTS_DOCS (`0` if
/// absent).
fn read_doc_len(
    docs: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<u32, TopoError> {
    let key = slot_key(slot);
    match docs.get(key.as_slice()).map_err(storage_err)? {
        Some(v) => {
            postcard::from_bytes::<u32>(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))
        }
        None => Ok(0),
    }
}

/// Transition node `slot`'s indexed text from `old_text` to `new_text`,
/// entirely within the caller's write transaction. Removes the node from
/// every term it no longer contains, re-sets its term frequency for every
/// term it now contains, rewrites its FTS_DOCS length, and folds `scope_id`'s
/// corpus stats in FTS_STATS. A no-op when `old_text == new_text`.
///
/// Postings are keyed per scope (`chunked_posting_key(scope_id, term,
/// chunk)`) and corpus stats are keyed per scope (`scope_id.to_be_bytes()`),
/// so this node's edits
/// touch only its own scope's df/doc-count/total-length — never any other
/// scope's. A node's scope is immutable, so `scope_id` is the same across an
/// update.
///
/// The stats move by *state transition*, not by naive addition:
/// - `None -> Some`: a new document (`doc_count += 1`, `total_len += new_len`).
/// - `Some -> Some`: an update in place (`doc_count` unchanged, `total_len`
///   adjusted by the length delta — this is where doc-count-vs-length drift
///   would creep in if handled carelessly).
/// - `Some -> None`: a removed document (`doc_count -= 1`, `total_len -=
///   old_len`, both saturating so stats can never go negative; a scope whose
///   last doc is removed drops its FTS_STATS row).
///
/// Factored so `rebuild_state_from_ops` and `migrate_v3::migrate_v2_to_v3`
/// reuse it verbatim during replay/migration.
pub(crate) fn fts_update(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    docs: &mut Table<'_, &'static [u8], &'static [u8]>,
    stats: &mut Table<'_, &'static [u8], &'static [u8]>,
    scope_id: u32,
    slot: u64,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> Result<(), TopoError> {
    // Tokenize each text exactly once, up front, and reuse the token vectors
    // below for the emptiness check, postings updates, and doc length — no
    // text is ever re-tokenized.
    let old_tokens = old_text.map(tokenize).unwrap_or_default();
    let new_tokens = new_text.map(tokenize).unwrap_or_default();

    // A "document" is text with >= 1 token. A declared prop holding "" (or
    // pure punctuation) must not inflate n_docs / deflate avgdl for everyone
    // else's scores. Normalizing HERE makes all four call paths (apply,
    // replay, reindex, and migration) agree by construction.
    let old_text = if old_tokens.is_empty() {
        None
    } else {
        old_text
    };
    let new_text = if new_tokens.is_empty() {
        None
    } else {
        new_text
    };

    if old_text == new_text {
        return Ok(());
    }

    let old_tf = term_freqs(&old_tokens);
    let new_tf = term_freqs(&new_tokens);

    // Union of affected terms: any term in the old OR new text. For each, the
    // node's desired frequency is its new count (0 → drop the node from that
    // term). This covers removals, insertions, and terms carried across.
    let mut terms: BTreeSet<&str> = BTreeSet::new();
    terms.extend(old_tf.keys().copied());
    terms.extend(new_tf.keys().copied());
    for term in terms {
        let count = new_tf.get(term).copied().unwrap_or(0);
        set_posting(postings, scope_id, term, slot, count)?;
    }

    let key = slot_key(slot);
    let old_len = old_tokens.len() as u64;
    let new_len = new_tokens.len() as u64;
    let (mut doc_count, mut total_len) = read_stats(stats, scope_id)?;
    match (old_text.is_some(), new_text.is_some()) {
        (false, true) => {
            doc_count += 1;
            total_len += new_len;
        }
        (true, true) => {
            total_len = total_len.saturating_sub(old_len) + new_len;
        }
        (true, false) => {
            doc_count = doc_count.saturating_sub(1);
            total_len = total_len.saturating_sub(old_len);
        }
        // Unreachable given the `old_text == new_text` guard (both None).
        (false, false) => {}
    }

    if new_text.is_some() {
        let bytes = postcard::to_allocvec(&(new_len as u32))
            .map_err(|e| TopoError::Encoding(e.to_string()))?;
        docs.insert(key.as_slice(), bytes.as_slice())
            .map_err(storage_err)?;
    } else {
        docs.remove(key.as_slice()).map_err(storage_err)?;
    }
    write_stats(stats, scope_id, doc_count, total_len)?;
    Ok(())
}

// -- v4 chunked postings block codec + maintenance (Task 4 codec, Task 6 --
// -- wiring) -----------------------------------------------------------
//
// Replaces the old single-row-per-term postings (`posting_key`/
// `encode_postings`/`decode_postings`/old `read_posting`/old `set_posting`,
// all deleted by Task 6) with chunked storage, mirroring `adj.rs`'s
// `(prefix, chunk)` scheme so a single hot term's postings never grow into
// one unbounded value that must be fully rewritten on every touch.

/// Chunked postings block format tag, byte 0 of every encoded payload.
pub(crate) const POSTINGS_BLOCK_FORMAT_V0: u8 = 0x00;

/// Target encoded chunk size (bytes) a chunk is split at — re-benchmarked in
/// Task 9.
// Task 9 chunk-target experiment (BENCHMARKS.md): 4 KiB beat 8/16/32 KiB on
// BOTH append (~0.43 ms/doc vs 1.0-1.8 ms/doc) and edit-heavy (~445 us/edit
// vs 760-1160 us/edit) indexing cost at a 10k-doc corpus, and was tied for
// best (not worst) on search latency — smaller covering/last chunks cost
// less to decode+re-encode per touch, and this workload's postings never
// get large enough for the extra chunk-count overhead to dominate reads.
pub(crate) const POSTINGS_CHUNK_TARGET: usize = 4 * 1024;

/// Chunked postings key for `term` under `scope_id`, chunk index `chunk`:
/// `scope_id.to_be_bytes() ++ term-UTF-8 ++ chunk.to_be_bytes()`. Both the
/// 4-byte scope prefix and the 4-byte chunk suffix are fixed-width, so the
/// only variable-length part is the term itself, always sandwiched between
/// two fixed-width fields. Two keys can only be byte-equal if their terms
/// are byte-equal too: a shorter and a longer term under the same scope
/// produce keys of different total length (`4 + term.len() + 4`), so no
/// chunk value for either can ever collide with the other.
pub(crate) fn chunked_posting_key(scope_id: u32, term: &str, chunk: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + term.len() + 4);
    key.extend_from_slice(&scope_id.to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key.extend_from_slice(&chunk.to_be_bytes());
    key
}

/// Encodes one postings chunk's `(slot, tf)` entries, ascending by slot, as
/// `[block_format][count varint]` then per entry `[slot_delta
/// varint][tf varint]` (first delta relative to 0) — the same delta-varint
/// shape as `adj::encode_block`. `entries` must be non-decreasing by slot
/// (slots are unique per term in practice, so strictly increasing, but only
/// non-decreasing is enforced here). An empty slice is always an error:
/// empty chunks are removed rather than ever written, so the encoder never
/// needs to represent one.
pub(crate) fn encode_posting_block(entries: &[(u64, u32)]) -> Result<Vec<u8>, TopoError> {
    if entries.is_empty() {
        return Err(TopoError::Encoding(
            "cannot encode an empty postings chunk (empty chunks are removed, never written)"
                .into(),
        ));
    }
    if entries.windows(2).any(|pair| pair[0].0 > pair[1].0) {
        return Err(TopoError::Encoding(
            "postings chunk entries are not slot-sorted".into(),
        ));
    }
    let mut out = Vec::new();
    out.push(POSTINGS_BLOCK_FORMAT_V0);
    write_varint(&mut out, entries.len() as u64);
    let mut previous = 0u64;
    for &(slot, tf) in entries {
        write_varint(
            &mut out,
            slot.checked_sub(previous)
                .ok_or_else(|| TopoError::Encoding("postings chunk slot underflow".into()))?,
        );
        previous = slot;
        write_varint(&mut out, tf as u64);
    }
    Ok(out)
}

/// Inverse of `encode_posting_block`. Rejects an unrecognised block format
/// tag and trailing bytes past the last decoded entry — same failure modes
/// as `adj::decode_block`.
pub(crate) fn decode_posting_block(payload: &[u8]) -> Result<Vec<(u64, u32)>, TopoError> {
    let Some((&format, mut input)) = payload.split_first() else {
        return Err(TopoError::Encoding("empty postings chunk block".into()));
    };
    if format != POSTINGS_BLOCK_FORMAT_V0 {
        return Err(TopoError::Encoding(format!(
            "unknown postings block format 0x{format:02X}"
        )));
    }
    let count = usize::try_from(read_varint(&mut input)?)
        .map_err(|_| TopoError::Encoding("postings chunk count too large".into()))?;
    let mut entries = Vec::with_capacity(count);
    let mut slot = 0u64;
    for _ in 0..count {
        slot = slot
            .checked_add(read_varint(&mut input)?)
            .ok_or_else(|| TopoError::Encoding("postings chunk slot overflow".into()))?;
        let tf = u32::try_from(read_varint(&mut input)?)
            .map_err(|_| TopoError::Encoding("postings chunk tf too large".into()))?;
        entries.push((slot, tf));
    }
    if !input.is_empty() {
        return Err(TopoError::Encoding(
            "trailing bytes in postings chunk block".into(),
        ));
    }
    Ok(entries)
}

/// Reads just `[block_format][count]` — the df fast path — without decoding
/// any entries. Errors identically to `decode_posting_block` on an empty
/// payload or an unrecognised format tag, so a corrupt block is caught the
/// same way regardless of which of the two readers touches it first.
pub(crate) fn posting_block_count(payload: &[u8]) -> Result<u64, TopoError> {
    let Some((&format, mut input)) = payload.split_first() else {
        return Err(TopoError::Encoding("empty postings chunk block".into()));
    };
    if format != POSTINGS_BLOCK_FORMAT_V0 {
        return Err(TopoError::Encoding(format!(
            "unknown postings block format 0x{format:02X}"
        )));
    }
    read_varint(&mut input)
}

/// Bounded range scan for `term`'s chunk keys under `scope_id`, returned in
/// ascending chunk order (the same order the keys sort in, since the chunk
/// suffix is fixed-width BE) — **never** a table iteration. The scan range
/// `[chunked_posting_key(scope_id, term, 0), chunked_posting_key(scope_id,
/// term, u32::MAX)]` can also contain a DIFFERENT term's chunk keys (a
/// longer or shorter term whose bytes happen to sort inside that byte
/// range — see `chunked_posting_key`'s key-length argument), so every
/// candidate key is checked against the exact expected length `4 +
/// term.len() + 4` before being kept. That length check, not the range
/// bound alone, is what makes the scan exact.
fn term_chunk_keys(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    scope_id: u32,
    term: &str,
) -> Result<Vec<Vec<u8>>, TopoError> {
    let start = chunked_posting_key(scope_id, term, 0);
    let end = chunked_posting_key(scope_id, term, u32::MAX);
    let want_len = start.len();
    let mut keys = Vec::new();
    for item in postings
        .range(start.as_slice()..=end.as_slice())
        .map_err(storage_err)?
    {
        let (key, _) = item.map_err(storage_err)?;
        let k = key.value();
        if k.len() == want_len {
            keys.push(k.to_vec());
        }
    }
    Ok(keys)
}

/// Recovers a chunk key's trailing 4-byte BE chunk index. Safe on any key
/// `term_chunk_keys` returned (every such key already passed the exact
/// `4 + term.len() + 4` length check).
fn chunk_number(key: &[u8]) -> u32 {
    let n = key.len();
    u32::from_be_bytes(
        key[n - 4..]
            .try_into()
            .expect("chunk key ends in a 4-byte BE chunk index"),
    )
}

/// Decodes one chunk's entries (empty if the key is absent — used only
/// defensively, since every key `term_chunk_keys` returns is present by
/// construction).
fn load_posting_chunk(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    key: &[u8],
) -> Result<Vec<(u64, u32)>, TopoError> {
    match postings.get(key).map_err(storage_err)? {
        Some(v) => {
            let raw = unframe_value(v.value())?;
            decode_posting_block(raw.as_ref())
        }
        None => Ok(Vec::new()),
    }
}

/// Encodes and writes one chunk's entries. `entries` must be non-empty —
/// callers remove the key instead of storing an empty chunk (the empty-key
/// doctrine `set_posting` relies on).
fn store_posting_chunk(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    key: &[u8],
    entries: &[(u64, u32)],
) -> Result<(), TopoError> {
    let framed = frame_value(encode_posting_block(entries)?);
    postings
        .insert(key, framed.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

/// Reads every chunk of `term`'s postings under `scope_id`, decoded and
/// concatenated in chunk order — which is also slot-ascending order, since
/// chunk slot ranges never overlap (each split cuts the sorted list at a
/// midpoint entry, and `set_posting`'s covering-chunk search relies on the
/// same non-overlap). Used by BM25 scoring, which needs every `(slot, tf)`
/// pair regardless of how many chunks they're spread across.
///
/// `pub(crate)` (like `set_posting`) so `migrate_v4.rs`'s tests can assert
/// the re-chunked postings round-trip correctly without duplicating a
/// second reader.
pub(crate) fn read_posting(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    scope_id: u32,
    term: &str,
) -> Result<Vec<(u64, u32)>, TopoError> {
    let mut out = Vec::new();
    for key in term_chunk_keys(postings, scope_id, term)? {
        out.extend(load_posting_chunk(postings, &key)?);
    }
    Ok(out)
}

/// Document frequency for `term` under `scope_id`: the sum of each chunk's
/// `posting_block_count` header, across every chunk — the df fast path,
/// never decoding an entry. `search_text` calls this before `read_posting`
/// so a term with zero matches in a scope (the common case for most query
/// terms against most scopes) is recognized without decoding anything.
///
/// `pub(crate)` — see `read_posting`'s identical visibility rationale.
pub(crate) fn posting_df(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    scope_id: u32,
    term: &str,
) -> Result<u64, TopoError> {
    let mut total = 0u64;
    for key in term_chunk_keys(postings, scope_id, term)? {
        if let Some(v) = postings.get(key.as_slice()).map_err(storage_err)? {
            let raw = unframe_value(v.value())?;
            total += posting_block_count(raw.as_ref())?;
        }
    }
    Ok(total)
}

/// Applies `(slot, count)` to one already-decoded covering chunk: update an
/// existing entry's tf, remove it (`count == 0`), or insert a genuinely new
/// slot at its sorted position, then rewrite the chunk (or remove its key,
/// if now empty — the empty-key doctrine). A `count == 0` for a slot absent
/// from this chunk is a no-op. No split check here — splitting is scoped to
/// `set_posting`'s append fast path, matching the design spec; a
/// covering-chunk insert can grow a chunk slightly past
/// `POSTINGS_CHUNK_TARGET` without triggering a split, which is an
/// accepted, deliberate simplification (re-visited if Task 9's benchmarking
/// says otherwise).
fn mutate_posting_chunk(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    key: &[u8],
    mut entries: Vec<(u64, u32)>,
    slot: u64,
    count: u32,
) -> Result<(), TopoError> {
    match entries.binary_search_by_key(&slot, |&(s, _)| s) {
        Ok(at) => {
            if count == 0 {
                entries.remove(at);
            } else {
                entries[at].1 = count;
            }
        }
        Err(at) => {
            if count == 0 {
                return Ok(()); // absent here; nothing to remove.
            }
            entries.insert(at, (slot, count));
        }
    }
    if entries.is_empty() {
        postings.remove(key).map_err(storage_err)?;
    } else {
        store_posting_chunk(postings, key, &entries)?;
    }
    Ok(())
}

/// Sets `slot`'s term-frequency for `term` (under `scope_id`) to `count`,
/// removing the entry (and its chunk key, if the chunk becomes empty; and
/// the whole term, if its last chunk does) when `count == 0`.
///
/// One bounded range scan locates the term's chunk keys (ascending chunk
/// order, which is also ascending slot-range order — see `read_posting`'s
/// doc comment), then the LAST chunk — and only the last chunk — is decoded
/// first:
/// - `slot` beyond the last chunk's max: the **fast path**, and the common
///   case, since new documents carry the highest slots. Append to the
///   already-decoded last chunk and split into `(chunk, chunk + 1)` at the
///   midpoint entry if the re-encoded chunk now exceeds
///   `POSTINGS_CHUNK_TARGET`. Exactly ONE chunk is decoded regardless of
///   how many the term has — O(1) per new document, which is the entire
///   point of chunking (the v3 single-row layout made this O(df)).
///   (`count == 0` for such a slot is a no-op: it's absent everywhere.)
/// - `slot` within the last chunk's own `[first, last]` range: the last
///   chunk IS the covering chunk (chunk slot ranges never overlap — every
///   earlier chunk's max is strictly below this chunk's first), so mutate
///   it in place via `mutate_posting_chunk`. Still exactly one chunk
///   decoded.
/// - `slot` below the last chunk's first entry: an update/remove of an
///   older document, or an out-of-order insert (a node's text can be
///   edited to newly include a term a later-created, higher-slot node
///   already carries). Scan the EARLIER chunks front-to-back, decoding
///   each until the first whose last entry's slot is `>= slot` — that's
///   the covering chunk (or, if every earlier chunk tops out below `slot`,
///   `slot` falls in the gap before the last chunk's range and the last
///   chunk covers it). The decode-forward here is inherent — a chunk's
///   slot range is only knowable by decoding it — and matches the spec's
///   weaker promise for updates ("scan the term's chunk keys, decode only
///   the covering chunk" — nothing past the covering chunk is decoded).
///
/// `pub(crate)` so `migrate_v4.rs`'s v3 -> v4 postings re-chunking pass can
/// drive the exact same incremental, tested chunk-splitting logic one old
/// single-row entry at a time (ascending by slot — the order those old rows
/// already carried on disk), rather than re-deriving chunk-split points from
/// scratch.
pub(crate) fn set_posting(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    scope_id: u32,
    term: &str,
    slot: u64,
    count: u32,
) -> Result<(), TopoError> {
    let keys = term_chunk_keys(postings, scope_id, term)?;

    let Some(last_key) = keys.last() else {
        if count == 0 {
            return Ok(()); // term doesn't exist; nothing to remove.
        }
        let key = chunked_posting_key(scope_id, term, 0);
        return store_posting_chunk(postings, &key, &[(slot, count)]);
    };

    // Decode ONLY the last chunk up front — the fast path must never touch
    // (or even decode) any other chunk.
    let mut last_entries = load_posting_chunk(postings, last_key)?;
    let last_max = last_entries
        .last()
        .expect("a stored chunk key is never empty")
        .0;

    if slot > last_max {
        // Fast path: beyond every chunk's range (chunks are slot-range
        // ordered, so beyond the last chunk's max means beyond them all).
        if count == 0 {
            return Ok(()); // absent everywhere; nothing to remove.
        }
        last_entries.push((slot, count));
        let chunk = chunk_number(last_key);
        return if encode_posting_block(&last_entries)?.len() <= POSTINGS_CHUNK_TARGET {
            store_posting_chunk(postings, last_key, &last_entries)
        } else {
            let split = last_entries.len() / 2;
            store_posting_chunk(postings, last_key, &last_entries[..split])?;
            let next_key = chunked_posting_key(scope_id, term, chunk + 1);
            store_posting_chunk(postings, &next_key, &last_entries[split..])
        };
    }

    let last_min = last_entries
        .first()
        .expect("a stored chunk key is never empty")
        .0;
    if slot >= last_min {
        // Within the last chunk's own range: it IS the covering chunk (no
        // earlier chunk's range can overlap it), so skip the earlier-chunk
        // scan entirely.
        return mutate_posting_chunk(postings, last_key, last_entries, slot, count);
    }

    // slot precedes the last chunk's range: scan the earlier chunks
    // front-to-back for the first whose range reaches slot.
    for key in &keys[..keys.len() - 1] {
        let entries = load_posting_chunk(postings, key)?;
        let chunk_max = entries.last().expect("a stored chunk key is never empty").0;
        if chunk_max >= slot {
            return mutate_posting_chunk(postings, key, entries, slot, count);
        }
        // This chunk's range ends before slot — keep scanning.
    }

    // Every earlier chunk tops out below slot, and slot < last_min: slot
    // falls in the gap just before the last chunk's range, so the last
    // chunk is the first whose max reaches it — it covers the insert.
    mutate_posting_chunk(postings, last_key, last_entries, slot, count)
}

impl Db {
    /// Scoped BM25 full-text search over the declared text index.
    ///
    /// Scores are computed within each scope's own corpus: df, doc count, and
    /// average document length are all per-scope, read from that scope's
    /// FTS_STATS row and scope-prefixed postings. Adding documents to scope B
    /// never changes scope A's scores. Each requested scope is scored
    /// independently and the results are merged (a node lives in exactly one
    /// scope, so the score maps never collide across scopes).
    ///
    /// `Rejected` if `k == 0` or the query tokenizes to nothing. For each scope
    /// in `scopes` and each distinct query term, reads that scope's postings
    /// and accumulates the BM25 contribution per document; `avgdl = total_len /
    /// n_docs` per scope (a scope with no corpus row is skipped, as is a scope
    /// never seen by the scope registry — it can't have any documents either).
    /// Hits are mapped to `NodeRecord`s by slot, straight out of the SAME read
    /// transaction the postings/stats were read from (post-W2a NODES is
    /// slot-keyed, so this is a direct `nodes[slot_key]` get — no ULID hop, no
    /// separate snapshot read), sorted by descending score (id as a
    /// deterministic tie-break), truncated to `k`, and bumped (access
    /// counters).
    pub fn search_text(
        &self,
        scopes: &ScopeSet,
        query: &str,
        k: usize,
    ) -> Result<Vec<(NodeRecord, f32)>, TopoError> {
        if k == 0 {
            return Err(TopoError::Rejected("text search requires k > 0".into()));
        }
        let tokens = tokenize(query);
        if tokens.is_empty() {
            return Err(TopoError::Rejected("query has no searchable terms".into()));
        }
        // Distinct terms — a repeated query word reads the same postings and
        // must not double-count.
        let distinct: BTreeSet<String> = tokens.into_iter().collect();

        let storage = self.storage();
        let tx = storage.db.begin_read().map_err(storage_err)?;
        let postings = tx.open_table(POSTINGS).map_err(storage_err)?;
        let docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
        let stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let vectors = tx.open_table(VECTORS).map_err(storage_err)?;
        let embedding_ref = tx.open_table(EMBEDDING_REF).map_err(storage_err)?;
        let dicts = storage.dicts.read().expect("dict lock poisoned");
        let scope_registry = storage
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");

        // Score each requested scope against its own corpus, then merge. A node
        // lives in exactly one scope, so its entry is only ever written under
        // that scope's pass — no cross-scope key collision in `scores`.
        let mut scores: HashMap<u64, f32> = HashMap::new();
        for scope in scopes.iter_scopes() {
            // A scope the registry has never interned has never had a node
            // written under it, so it has no FTS_STATS row and no postings —
            // skip without touching the tables, same outcome as `n_docs == 0`
            // below for a scope that WAS interned but is currently empty.
            let Some(scope_id) = scope_registry.id_of(scope) else {
                continue;
            };
            let (n_docs, total_len) = read_stats(&stats, scope_id)?;
            if n_docs == 0 {
                continue;
            }
            let avgdl = total_len as f32 / n_docs as f32;
            for term in &distinct {
                // df fast path first: most query terms miss most scopes, and
                // this sums each chunk's block-count header without
                // decoding a single entry — skip the full decode below
                // entirely when there's nothing to score.
                let df = posting_df(&postings, scope_id, term)? as f32;
                if df == 0.0 {
                    continue;
                }
                let list = read_posting(&postings, scope_id, term)?;
                let idf = ((n_docs as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
                for (slot, tf) in list {
                    let len = read_doc_len(&docs, slot)? as f32;
                    let tf = tf as f32;
                    let denom = tf + K1 * (1.0 - B + B * len / avgdl);
                    *scores.entry(slot).or_insert(0.0) += idf * tf * (K1 + 1.0) / denom;
                }
            }
        }

        let mut out: Vec<(NodeRecord, f32)> = Vec::with_capacity(scores.len());
        for (slot, score) in scores {
            if let Some(rec) = read_node_by_slot(
                &nodes,
                &vectors,
                &embedding_ref,
                &dicts,
                &scope_registry,
                slot,
            )? {
                // Defensive only, not load-bearing for isolation: postings
                // are already scope-prefixed (see `chunked_posting_key`), so
                // every `slot` scored above already comes from a requested scope's
                // own postings list. This guards against a slot whose record
                // is corrupt/desynced from its own postings row, not against
                // cross-scope leakage.
                if scopes.contains(rec.scope) {
                    out.push((rec, score));
                }
            }
        }
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        out.truncate(k);
        self.bump(out.iter().map(|(n, _)| n.id));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use redb::Database;

    /// Pins the new postings value codec (delta-varint `(slot, tf)` pairs,
    /// ascending by slot, tf > 1 supported) and the new fixed-width 4-byte
    /// scope-id key prefix: scope ids `1` (BE `00 00 00 01`) and `256` (BE
    /// `00 00 01 00`) share their first two bytes, which is exactly the kind
    /// of near-collision a prefix-based key scheme must still keep isolated.
    /// Reuses the same slot (`2`) in both scopes on purpose — a key collision
    /// would show up here as a merged or overwritten posting.
    #[test]
    fn postings_roundtrip_deltas_and_isolate_be_sharing_scope_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            let mut docs = tx.open_table(FTS_DOCS).unwrap();
            let mut stats = tx.open_table(FTS_STATS).unwrap();

            // Scope 1: three docs sharing the term "rust" at non-contiguous
            // slots, one with tf == 2, so the round trip exercises both the
            // delta encoding (gaps between slots) and tf > 1.
            fts_update(
                &mut postings,
                &mut docs,
                &mut stats,
                1,
                2,
                None,
                Some("rust rust database"),
            )
            .unwrap();
            fts_update(
                &mut postings,
                &mut docs,
                &mut stats,
                1,
                5,
                None,
                Some("rust engine"),
            )
            .unwrap();
            fts_update(
                &mut postings,
                &mut docs,
                &mut stats,
                1,
                9,
                None,
                Some("rust topology graph"),
            )
            .unwrap();

            // Scope 256 — BE bytes share scope 1's first two bytes — reuses
            // slot 2 with a different tf, so a prefix bug would either merge
            // this into scope 1's slot-2 entry or read back the wrong tf.
            fts_update(
                &mut postings,
                &mut docs,
                &mut stats,
                256,
                2,
                None,
                Some("rust filler"),
            )
            .unwrap();

            let list_1 = read_posting(&postings, 1, "rust").unwrap();
            assert_eq!(
                list_1,
                vec![(2, 2), (5, 1), (9, 1)],
                "scope 1's postings must round-trip sorted by slot with the correct tf"
            );

            let list_256 = read_posting(&postings, 256, "rust").unwrap();
            assert_eq!(
                list_256,
                vec![(2, 1)],
                "scope 256's postings must stay isolated from scope 1's, despite sharing slot 2 and a BE-key prefix"
            );
        }
        tx.commit().unwrap();
    }

    // -- v4 chunked postings block codec (Task 4) ---------------------------

    #[test]
    fn posting_block_roundtrips_boundaries() {
        assert!(
            encode_posting_block(&[]).is_err(),
            "encoding an empty entry list must be an error — empty chunks are removed, never written"
        );

        let one = vec![(5u64, 3u32)];
        assert_eq!(
            decode_posting_block(&encode_posting_block(&one).unwrap()).unwrap(),
            one
        );

        let tf_gt_one = vec![(0u64, 1u32), (10, 7)];
        assert_eq!(
            decode_posting_block(&encode_posting_block(&tf_gt_one).unwrap()).unwrap(),
            tf_gt_one
        );

        let max_slot = vec![(u64::MAX, 1u32)];
        assert_eq!(
            decode_posting_block(&encode_posting_block(&max_slot).unwrap()).unwrap(),
            max_slot
        );

        // Equal adjacent slots: the encoder only requires non-decreasing, so
        // a repeated slot (delta 0 between equals) must round-trip exactly.
        let equal_slots = vec![(5u64, 1u32), (5, 2)];
        assert_eq!(
            decode_posting_block(&encode_posting_block(&equal_slots).unwrap()).unwrap(),
            equal_slots
        );
    }

    #[test]
    fn posting_block_rejects_bad_payloads() {
        assert!(
            encode_posting_block(&[(2, 1), (1, 1)]).is_err(),
            "decreasing slots must be rejected"
        );
        assert!(
            decode_posting_block(&[]).is_err(),
            "empty payload must be rejected"
        );
        assert!(
            decode_posting_block(&[0xFF]).is_err(),
            "unknown block format must be rejected"
        );

        let full = encode_posting_block(&[(0, 1), (5, 2), (9, 1)]).unwrap();
        for cut in 1..full.len() {
            assert!(
                decode_posting_block(&full[..cut]).is_err(),
                "truncation at byte {cut} must be rejected"
            );
        }
    }

    #[test]
    fn posting_block_count_matches_decode_len_without_full_decode() {
        let blocks: Vec<Vec<(u64, u32)>> = vec![
            vec![(0, 1)],
            vec![(0, 1), (3, 5), (100, 2)],
            vec![(u64::MAX, u32::MAX)],
            (0..50)
                .map(|i| (i as u64 * 2, (i % 7) as u32 + 1))
                .collect(),
        ];
        for entries in blocks {
            let payload = encode_posting_block(&entries).unwrap();
            let count = posting_block_count(&payload).unwrap();
            let decoded = decode_posting_block(&payload).unwrap();
            assert_eq!(count, decoded.len() as u64);
        }

        // The fast path must fail the same way decode does on an empty
        // payload and an unknown format byte — pinned independently, so a
        // refactor moving the format check after the count read is caught.
        assert!(
            posting_block_count(&[]).is_err(),
            "empty payload must be rejected by the count fast path"
        );
        assert!(
            posting_block_count(&[0xFF]).is_err(),
            "unknown block format must be rejected by the count fast path"
        );
    }

    /// "ab" vs "abc" under one scope: the trailing fixed 4-byte chunk field
    /// means the two keys' lengths (`4 + term_len + 4`) always differ, so no
    /// chunk value for either term can ever produce the other's key. Scope 1
    /// (BE `00 00 00 01`) and scope 256 (BE `00 00 01 00`) share their first
    /// two bytes but must still disambiguate under the same term. Chunk 0's
    /// key must sort strictly before chunk 1's under the same `(scope,
    /// term)`, so a bounded range scan returns chunks in order.
    #[test]
    fn chunked_posting_key_disambiguates_terms_scopes_and_chunks() {
        let ab = chunked_posting_key(1, "ab", 0);
        let abc = chunked_posting_key(1, "abc", 0);
        assert_eq!(ab.len(), 4 + "ab".len() + 4);
        assert_eq!(abc.len(), 4 + "abc".len() + 4);
        assert_ne!(ab, abc);

        let scope1 = chunked_posting_key(1, "rust", 0);
        let scope256 = chunked_posting_key(256, "rust", 0);
        assert_ne!(scope1, scope256);

        let chunk0 = chunked_posting_key(1, "rust", 0);
        let chunk1 = chunked_posting_key(1, "rust", 1);
        assert!(chunk0 < chunk1);
    }

    proptest! {
        #[test]
        fn sorted_posting_block_entries_roundtrip(
            mut entries in proptest::collection::vec((0u64..10_000, any::<u32>()), 1..64)
        ) {
            entries.sort_by_key(|entry| entry.0);
            prop_assert_eq!(
                decode_posting_block(&encode_posting_block(&entries).unwrap()).unwrap(),
                entries
            );
        }
    }

    // -- Task 6: chunked postings wiring (set_posting/read_posting rework) -

    /// A term with few docs must live entirely in chunk 0 — the common case,
    /// pinned separately from the split case below.
    #[test]
    fn three_docs_sharing_a_term_produce_one_chunk_with_three_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "rust", 2, 1).unwrap();
            set_posting(&mut postings, 1, "rust", 5, 1).unwrap();
            set_posting(&mut postings, 1, "rust", 9, 1).unwrap();
            let keys = term_chunk_keys(&postings, 1, "rust").unwrap();
            assert_eq!(keys.len(), 1, "3 small docs must fit in a single chunk");
            assert_eq!(
                read_posting(&postings, 1, "rust").unwrap(),
                vec![(2, 1), (5, 1), (9, 1)]
            );
            assert_eq!(posting_df(&postings, 1, "rust").unwrap(), 3);
        }
        tx.commit().unwrap();
    }

    /// Sequential inserts on one hot term force a real split at
    /// `POSTINGS_CHUNK_TARGET` (chosen over a tiny test-only target override
    /// — see the Task 6 report for the justification). `df` (the
    /// `posting_block_count`-summed fast path) must equal the total entry
    /// count across every chunk, and the concatenated read must stay
    /// slot-ascending even though it now spans >1 chunk.
    #[test]
    fn a_hot_term_splits_into_multiple_chunks_and_df_sums_across_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let n: u64 = 5000;
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for slot in 0..n {
                set_posting(&mut postings, 1, "hot", slot, 1).unwrap();
            }
            let keys = term_chunk_keys(&postings, 1, "hot").unwrap();
            assert!(
                keys.len() > 1,
                "{n} sequential docs on one term must split into >1 chunk, got {}",
                keys.len()
            );
            let df = posting_df(&postings, 1, "hot").unwrap();
            assert_eq!(df, n, "df must equal the total entry count across chunks");
            let all = read_posting(&postings, 1, "hot").unwrap();
            assert_eq!(all.len(), n as usize);
            assert!(
                all.windows(2).all(|w| w[0].0 < w[1].0),
                "concatenation across chunks must stay slot-ascending"
            );
        }
        tx.commit().unwrap();
    }

    /// Updating a doc's tf must rewrite only its own covering chunk — a
    /// sibling chunk's stored bytes must be byte-identical before and after.
    #[test]
    fn updating_one_docs_tf_touches_only_its_covering_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let n: u64 = 5000;
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for slot in 0..n {
                set_posting(&mut postings, 1, "hot", slot, 1).unwrap();
            }
        }
        tx.commit().unwrap();

        let keys_before = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            term_chunk_keys(&postings, 1, "hot").unwrap()
        };
        assert!(
            keys_before.len() > 1,
            "setup must produce a multi-chunk term"
        );
        let first_chunk_bytes_before = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            postings
                .get(keys_before[0].as_slice())
                .unwrap()
                .unwrap()
                .value()
                .to_vec()
        };

        // Update the highest slot's tf — guaranteed to live in the LAST
        // chunk, not the first.
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "hot", n - 1, 7).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(POSTINGS).unwrap();
        let first_chunk_bytes_after = postings
            .get(keys_before[0].as_slice())
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        assert_eq!(
            first_chunk_bytes_before, first_chunk_bytes_after,
            "a sibling chunk's stored bytes must be untouched by an update to a different chunk"
        );
        let all = read_posting(&postings, 1, "hot").unwrap();
        assert_eq!(
            all.iter().find(|&&(s, _)| s == n - 1).unwrap().1,
            7,
            "the updated slot's tf must actually change"
        );
    }

    /// Removing every doc from the term's trailing (smaller) chunk must drop
    /// exactly that chunk's key, leaving the sibling chunk untouched.
    #[test]
    fn removing_a_full_chunks_docs_drops_only_that_chunks_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let n: u64 = 5000;
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for slot in 0..n {
                set_posting(&mut postings, 1, "hot", slot, 1).unwrap();
            }
        }
        tx.commit().unwrap();

        let keys = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            term_chunk_keys(&postings, 1, "hot").unwrap()
        };
        assert!(keys.len() > 1, "setup must produce a multi-chunk term");
        let last_key = keys.last().unwrap().clone();
        let last_chunk_slots: Vec<u64> = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            load_posting_chunk(&postings, last_key.as_slice())
                .unwrap()
                .into_iter()
                .map(|(slot, _)| slot)
                .collect()
        };
        assert!(!last_chunk_slots.is_empty());

        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for slot in &last_chunk_slots {
                set_posting(&mut postings, 1, "hot", *slot, 0).unwrap();
            }
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(POSTINGS).unwrap();
        let remaining_keys = term_chunk_keys(&postings, 1, "hot").unwrap();
        assert_eq!(
            remaining_keys.len(),
            keys.len() - 1,
            "emptying the last chunk must drop exactly its own key"
        );
        assert!(
            !remaining_keys.contains(&last_key),
            "the emptied chunk's key must be gone"
        );
        assert_eq!(
            read_posting(&postings, 1, "hot").unwrap().len(),
            (n as usize) - last_chunk_slots.len()
        );
    }

    /// Removing every doc from a (single-chunk) term must make the term
    /// disappear entirely — no chunk keys left (empty-key doctrine).
    #[test]
    fn removing_every_doc_drops_the_whole_term() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "x", 1, 1).unwrap();
            set_posting(&mut postings, 1, "x", 2, 1).unwrap();
            set_posting(&mut postings, 1, "x", 3, 1).unwrap();
            set_posting(&mut postings, 1, "x", 1, 0).unwrap();
            set_posting(&mut postings, 1, "x", 2, 0).unwrap();
            assert_eq!(read_posting(&postings, 1, "x").unwrap(), vec![(3, 1)]);
            set_posting(&mut postings, 1, "x", 3, 0).unwrap();
            assert!(
                term_chunk_keys(&postings, 1, "x").unwrap().is_empty(),
                "the term must fully disappear once its last doc is removed"
            );
            assert_eq!(read_posting(&postings, 1, "x").unwrap(), vec![]);
            assert_eq!(posting_df(&postings, 1, "x").unwrap(), 0);
        }
        tx.commit().unwrap();
    }

    /// A term's max slot doesn't only grow via the fast append path: a node
    /// created long ago can have its text edited to newly include a term
    /// that a LATER (higher-slot) node already carries, so `set_posting`
    /// must route the smaller, brand-new slot into the correct covering
    /// chunk and keep it sorted — not silently append it out of order.
    #[test]
    fn out_of_order_insert_lands_sorted_within_the_covering_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "late", 10, 1).unwrap();
            set_posting(&mut postings, 1, "late", 3, 2).unwrap();
            assert_eq!(
                read_posting(&postings, 1, "late").unwrap(),
                vec![(3, 2), (10, 1)]
            );
        }
        tx.commit().unwrap();
    }

    /// The fast path's O(1) contract, pinned at multi-chunk scale: inserting
    /// a beyond-max slot into a term with >= 3 chunks must leave every
    /// non-last chunk's STORED BYTES byte-identical. That catches any
    /// accidental MUTATION of a sibling chunk; it cannot catch a pure
    /// read or a deterministic decode-and-rewrite (the codec + framing are
    /// deterministic, so a redundant rewrite reproduces the same bytes) —
    /// the decode-ONE-chunk guarantee itself is held by `set_posting`'s
    /// structure: only `keys.last()` is decoded before the fast-path branch
    /// returns. 10_000 sequential 2-byte entries split at
    /// `POSTINGS_CHUNK_TARGET` (~4095 entries), then every ~2047 more, so
    /// the setup produces >= 4 chunks with the REAL production target — no
    /// test-only override, same prod/test-parity rationale as the other
    /// multi-chunk tests here.
    #[test]
    fn fast_path_insert_leaves_every_non_last_chunks_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let n: u64 = 10_000;
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for slot in 0..n {
                set_posting(&mut postings, 1, "hot", slot, 1).unwrap();
            }
        }
        tx.commit().unwrap();

        let (keys, non_last_bytes_before) = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            let keys = term_chunk_keys(&postings, 1, "hot").unwrap();
            let bytes: Vec<Vec<u8>> = keys[..keys.len() - 1]
                .iter()
                .map(|key| {
                    postings
                        .get(key.as_slice())
                        .unwrap()
                        .unwrap()
                        .value()
                        .to_vec()
                })
                .collect();
            (keys, bytes)
        };
        assert!(
            keys.len() >= 3,
            "setup must produce >= 3 chunks, got {}",
            keys.len()
        );

        // The fast-path insert: a brand-new slot beyond every chunk's max.
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "hot", n, 1).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(POSTINGS).unwrap();
        for (i, (key, before)) in keys[..keys.len() - 1]
            .iter()
            .zip(&non_last_bytes_before)
            .enumerate()
        {
            let after = postings
                .get(key.as_slice())
                .unwrap()
                .unwrap()
                .value()
                .to_vec();
            assert_eq!(
                before, &after,
                "non-last chunk {i}'s stored bytes must be byte-identical after a fast-path insert"
            );
        }
        assert_eq!(
            posting_df(&postings, 1, "hot").unwrap(),
            n + 1,
            "the fast-path insert must actually land"
        );
    }

    /// Minor-1 regression (reviewer-requested): the covering chunk of an
    /// out-of-order insert at MULTI-chunk scale is an EARLIER, non-last
    /// chunk — pins the chunk-range non-overlap invariant `set_posting`'s
    /// covering search relies on. Even slots only, so every odd slot is a
    /// gap inside an existing chunk's range; the new odd slot must land
    /// sorted inside the first chunk while the LAST chunk's stored bytes
    /// stay byte-identical (proving the earlier chunk, not the last-chunk
    /// gap fallback, absorbed it) and the chunk count stays unchanged.
    #[test]
    fn out_of_order_insert_into_an_earlier_chunk_leaves_the_last_chunk_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let n: u64 = 6000; // even slots 0, 2, .., 11998
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            for i in 0..n {
                set_posting(&mut postings, 1, "hot", i * 2, 1).unwrap();
            }
        }
        tx.commit().unwrap();

        let (keys, first_chunk, last_bytes_before) = {
            let tx = db.begin_read().unwrap();
            let postings = tx.open_table(POSTINGS).unwrap();
            let keys = term_chunk_keys(&postings, 1, "hot").unwrap();
            let first_chunk = load_posting_chunk(&postings, keys[0].as_slice()).unwrap();
            let last_bytes = postings
                .get(keys.last().unwrap().as_slice())
                .unwrap()
                .unwrap()
                .value()
                .to_vec();
            (keys, first_chunk, last_bytes)
        };
        assert!(
            keys.len() >= 2,
            "setup must produce >= 2 chunks, got {}",
            keys.len()
        );
        // An odd slot strictly inside the FIRST chunk's [first, last] range —
        // a gap by construction (only even slots were inserted).
        let missing = first_chunk[0].0 + 1;
        assert!(missing < first_chunk.last().unwrap().0);

        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            set_posting(&mut postings, 1, "hot", missing, 5).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(POSTINGS).unwrap();
        let keys_after = term_chunk_keys(&postings, 1, "hot").unwrap();
        assert_eq!(
            keys_after.len(),
            keys.len(),
            "a covering-chunk insert must not add or remove chunk keys"
        );
        let last_bytes_after = postings
            .get(keys.last().unwrap().as_slice())
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        assert_eq!(
            last_bytes_before, last_bytes_after,
            "the LAST chunk's stored bytes must be untouched by an insert covered by an earlier chunk"
        );
        let all = read_posting(&postings, 1, "hot").unwrap();
        assert_eq!(all.len(), n as usize + 1);
        assert!(
            all.windows(2).all(|w| w[0].0 < w[1].0),
            "the out-of-order insert must land in sorted position"
        );
        assert!(
            all.contains(&(missing, 5)),
            "the inserted (slot, tf) must be present with its tf"
        );
    }
}
