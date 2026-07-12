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
//! v3 layout (re-keyed from the ULID/`Scope`-keyed v2 layout by W2b):
//! Postings are keyed by `scope_id.to_be_bytes() ++ term` (`scope_id` is the
//! [`crate::scopes::ScopeRegistry`]-interned `u32`, fixed-width so no term
//! can ever straddle two scopes' key ranges) and corpus stats live in
//! FTS_STATS keyed by that same 4-byte `scope_id`, so a document in one scope
//! never shifts another scope's df/avgdl. FTS_DOCS is keyed by the node's
//! 8-byte BE dense slot (matching NODES/EDGES/EMBEDDINGS/COUNTERS). Postings
//! values are delta-varint `(slot_delta, tf)` pairs, ascending by slot,
//! wrapped in the same `codec::frame_value` framing as other v3 values;
//! FTS_DOCS/FTS_STATS values are unchanged plain-postcard payloads.
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
use crate::storage::{
    read_node_by_slot, slot_key, EMBEDDINGS, FTS_DOCS, FTS_STATS, NODES, POSTINGS,
};
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

/// Encodes a postings list as `[count varint]` then per entry, ascending by
/// slot, `[slot_delta varint][tf varint]` (first delta relative to 0).
/// Reuses `adj.rs`'s general-purpose varint helpers — same trick as the
/// adjacency block codec (`adj::encode_block`/`decode_block`).
fn encode_postings(entries: &[(u64, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, entries.len() as u64);
    let mut previous = 0u64;
    for &(slot, tf) in entries {
        write_varint(&mut out, slot - previous);
        previous = slot;
        write_varint(&mut out, tf as u64);
    }
    out
}

/// Inverse of `encode_postings`. Rejects trailing bytes so a truncated or
/// corrupt value is caught here rather than silently under-reading.
fn decode_postings(payload: &[u8]) -> Result<Vec<(u64, u32)>, TopoError> {
    let mut input = payload;
    let count = usize::try_from(read_varint(&mut input)?)
        .map_err(|_| TopoError::Encoding("postings count too large".into()))?;
    let mut entries = Vec::with_capacity(count);
    let mut slot = 0u64;
    for _ in 0..count {
        slot = slot
            .checked_add(read_varint(&mut input)?)
            .ok_or_else(|| TopoError::Encoding("postings slot overflow".into()))?;
        let tf = u32::try_from(read_varint(&mut input)?)
            .map_err(|_| TopoError::Encoding("postings tf too large".into()))?;
        entries.push((slot, tf));
    }
    if !input.is_empty() {
        return Err(TopoError::Encoding(
            "trailing bytes in postings value".into(),
        ));
    }
    Ok(entries)
}

/// Reads the postings list for `term_key` (empty if absent) as
/// `Vec<(slot, tf)>`, ascending by slot.
fn read_posting(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    term_key: &[u8],
) -> Result<Vec<(u64, u32)>, TopoError> {
    match postings.get(term_key).map_err(storage_err)? {
        Some(v) => {
            let raw = unframe_value(v.value())?;
            decode_postings(raw.as_ref())
        }
        None => Ok(Vec::new()),
    }
}

/// Sets `slot`'s term-frequency in `term_key`'s postings to `count`, removing
/// the node's entry (and the whole postings key, if it becomes empty) when
/// `count == 0`. Maintained via a `BTreeMap<u64, u32>` so the re-encoded
/// delta-varint list is always sorted ascending by slot — deterministic on
/// disk regardless of update order.
fn set_posting(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    term_key: &[u8],
    slot: u64,
    count: u32,
) -> Result<(), TopoError> {
    let mut map: BTreeMap<u64, u32> = match postings.get(term_key).map_err(storage_err)? {
        Some(v) => {
            let raw = unframe_value(v.value())?;
            decode_postings(raw.as_ref())?.into_iter().collect()
        }
        None => BTreeMap::new(),
    };
    if count == 0 {
        map.remove(&slot);
    } else {
        map.insert(slot, count);
    }
    if map.is_empty() {
        // Drop empty postings keys (same empty-key doctrine as the prop index).
        postings.remove(term_key).map_err(storage_err)?;
    } else {
        let entries: Vec<(u64, u32)> = map.into_iter().collect();
        let framed = frame_value(encode_postings(&entries));
        postings
            .insert(term_key, framed.as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}

/// Postings key for `term` under scope id `scope_id`: `scope_id.to_be_bytes()
/// ++ term-UTF-8`. The scope-id prefix is fixed-width (4 bytes), so no
/// separator is needed and no term can collide across scopes — even scope
/// ids whose BE bytes share a leading byte (e.g. `1` = `00 00 00 01` and
/// `256` = `00 00 01 00`) never produce overlapping keys, since the prefix
/// is always exactly 4 bytes before the term starts.
fn posting_key(scope_id: u32, term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + term.len());
    key.extend_from_slice(&scope_id.to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key
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
/// Postings are keyed per scope (`posting_key(scope_id, term)`) and corpus
/// stats are keyed per scope (`scope_id.to_be_bytes()`), so this node's edits
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
        set_posting(
            postings,
            posting_key(scope_id, term).as_slice(),
            slot,
            count,
        )?;
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
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
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
                let list = read_posting(&postings, posting_key(scope_id, term).as_slice())?;
                let df = list.len() as f32;
                if df == 0.0 {
                    continue;
                }
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
            if let Some(rec) =
                read_node_by_slot(&nodes, &embeddings, &dicts, &scope_registry, slot)?
            {
                // Defensive only, not load-bearing for isolation: postings
                // are already scope-prefixed (see `posting_key`), so every
                // `slot` scored above already comes from a requested scope's
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

            let list_1 = read_posting(&postings, posting_key(1, "rust").as_slice()).unwrap();
            assert_eq!(
                list_1,
                vec![(2, 2), (5, 1), (9, 1)],
                "scope 1's postings must round-trip sorted by slot with the correct tf"
            );

            let list_256 = read_posting(&postings, posting_key(256, "rust").as_slice()).unwrap();
            assert_eq!(
                list_256,
                vec![(2, 1)],
                "scope 256's postings must stay isolated from scope 1's, despite sharing slot 2 and a BE-key prefix"
            );
        }
        tx.commit().unwrap();
    }
}
