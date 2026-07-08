//! Full-text search: tokenization, per-node declared text extraction,
//! transactional per-scope postings maintenance, and scoped BM25 ranking.
//!
//! The postings/doc-length/per-scope-corpus tables are maintained *inside the
//! same redb write transaction* as the graph state, via [`fts_update`] — so a
//! batch is atomic across NODES/EDGES **and** the text index, and a rejected
//! batch leaves the index untouched. `apply_batch` collects
//! `(scope, id, old_text, new_text)` during its op loop and drives `fts_update`
//! after every op succeeds; `rebuild_state_from_ops` reuses the very same
//! function during replay (after draining POSTINGS/FTS_DOCS/FTS_STATS).
//! Postings are keyed by `scope_key(scope) ++ term` and corpus stats live in
//! FTS_STATS keyed by scope, so a document in one scope never shifts another
//! scope's df/avgdl. `search_text` reads the committed tables through a fresh
//! read transaction, scores BM25 within each requested scope's own corpus, and
//! merges (a node lives in exactly one scope — no cross-scope key collision).

use crate::db::Db;
use crate::error::TopoError;
use crate::ids::{NodeId, Scope, ScopeSet};
use crate::index::IndexSpec;
use crate::props::PropValue;
use crate::state::NodeRecord;
use crate::storage::{node_key, scope_key, FTS_DOCS, FTS_STATS, POSTINGS};
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

/// Reads the postings list for `term` (empty if absent) as `Vec<(NodeId, u32)>`.
fn read_posting(
    postings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    term: &[u8],
) -> Result<Vec<(NodeId, u32)>, TopoError> {
    match postings.get(term).map_err(redb::Error::from)? {
        Some(v) => postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string())),
        None => Ok(Vec::new()),
    }
}

/// Sets `id`'s term-frequency in `term`'s postings to `count`, removing the
/// node's entry (and the whole postings key, if it becomes empty) when
/// `count == 0`. Maintained via a `BTreeMap<NodeId, u32>` so the re-encoded
/// `Vec<(NodeId, u32)>` is always sorted by node id — deterministic on disk.
fn set_posting(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    term: &[u8],
    id: NodeId,
    count: u32,
) -> Result<(), TopoError> {
    let mut map: BTreeMap<NodeId, u32> = match postings.get(term).map_err(redb::Error::from)? {
        Some(v) => {
            let vec: Vec<(NodeId, u32)> =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            vec.into_iter().collect()
        }
        None => BTreeMap::new(),
    };
    if count == 0 {
        map.remove(&id);
    } else {
        map.insert(id, count);
    }
    if map.is_empty() {
        // Drop empty postings keys (same empty-key doctrine as the prop index).
        postings.remove(term).map_err(redb::Error::from)?;
    } else {
        let vec: Vec<(NodeId, u32)> = map.into_iter().collect();
        let bytes = postcard::to_allocvec(&vec).map_err(|e| TopoError::Encoding(e.to_string()))?;
        postings
            .insert(term, bytes.as_slice())
            .map_err(redb::Error::from)?;
    }
    Ok(())
}

/// Postings key for `term` in `scope`: `scope_key(scope) ++ term-UTF-8`. The
/// scope prefix is fixed-width (17 bytes), so no separator is needed and no
/// term can collide across scopes (one scope's key is never a prefix of
/// another's).
fn posting_key(scope: Scope, term: &str) -> Vec<u8> {
    let prefix = scope_key(scope);
    let mut key = Vec::with_capacity(prefix.len() + term.len());
    key.extend_from_slice(&prefix);
    key.extend_from_slice(term.as_bytes());
    key
}

/// Reads a scope's `(doc_count, total_len)` corpus stats from FTS_STATS
/// (`(0, 0)` if absent).
fn read_stats(
    stats: &impl ReadableTable<&'static [u8], &'static [u8]>,
    scope: Scope,
) -> Result<(u64, u64), TopoError> {
    let key = scope_key(scope);
    match stats.get(key.as_slice()).map_err(redb::Error::from)? {
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
    scope: Scope,
    doc_count: u64,
    total_len: u64,
) -> Result<(), TopoError> {
    let key = scope_key(scope);
    if doc_count == 0 {
        stats.remove(key.as_slice()).map_err(redb::Error::from)?;
    } else {
        let bytes = postcard::to_allocvec(&(doc_count, total_len))
            .map_err(|e| TopoError::Encoding(e.to_string()))?;
        stats
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(redb::Error::from)?;
    }
    Ok(())
}

/// The doc length (in tokens) recorded for `id` in FTS_DOCS (`0` if absent).
fn read_doc_len(
    docs: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<u32, TopoError> {
    let key = node_key(id);
    match docs.get(key.as_slice()).map_err(redb::Error::from)? {
        Some(v) => {
            postcard::from_bytes::<u32>(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))
        }
        None => Ok(0),
    }
}

/// Transition node `id`'s indexed text from `old_text` to `new_text`, entirely
/// within the caller's write transaction. Removes the node from every term it
/// no longer contains, re-sets its term frequency for every term it now
/// contains, rewrites its FTS_DOCS length, and folds `scope`'s corpus stats in
/// FTS_STATS. A no-op when `old_text == new_text`.
///
/// Postings are keyed per scope (`posting_key(scope, term)`) and corpus stats
/// are keyed per scope (`scope_key(scope)`), so this node's edits touch only
/// its own scope's df/doc-count/total-length — never any other scope's. A
/// node's scope is immutable, so `scope` is the same across an update.
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
/// Factored so `rebuild_state_from_ops` reuses it verbatim during replay.
pub(crate) fn fts_update(
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    docs: &mut Table<'_, &'static [u8], &'static [u8]>,
    stats: &mut Table<'_, &'static [u8], &'static [u8]>,
    scope: Scope,
    id: NodeId,
    old_text: Option<&str>,
    new_text: Option<&str>,
) -> Result<(), TopoError> {
    // A "document" is text with >= 1 token. A declared prop holding "" (or
    // pure punctuation) must not inflate n_docs / deflate avgdl for everyone
    // else's scores. Normalizing HERE makes all four call paths (apply,
    // replay, reindex, and Task 5's per-scope rework) agree by construction.
    let old_text = old_text.filter(|t| !tokenize(t).is_empty());
    let new_text = new_text.filter(|t| !tokenize(t).is_empty());

    if old_text == new_text {
        return Ok(());
    }

    let old_tokens = old_text.map(tokenize).unwrap_or_default();
    let new_tokens = new_text.map(tokenize).unwrap_or_default();
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
        set_posting(postings, posting_key(scope, term).as_slice(), id, count)?;
    }

    let key = node_key(id);
    let old_len = old_tokens.len() as u64;
    let new_len = new_tokens.len() as u64;
    let (mut doc_count, mut total_len) = read_stats(stats, scope)?;
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
            .map_err(redb::Error::from)?;
    } else {
        docs.remove(key.as_slice()).map_err(redb::Error::from)?;
    }
    write_stats(stats, scope, doc_count, total_len)?;
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
    /// n_docs` per scope (a scope with no corpus row is skipped). Hits are
    /// mapped through a single snapshot, sorted by descending score (id as a
    /// deterministic tie-break), truncated to `k`, and bumped (access counters,
    /// Task 4).
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
        let tx = storage.db.begin_read().map_err(redb::Error::from)?;
        let postings = tx.open_table(POSTINGS).map_err(redb::Error::from)?;
        let docs = tx.open_table(FTS_DOCS).map_err(redb::Error::from)?;
        let stats = tx.open_table(FTS_STATS).map_err(redb::Error::from)?;

        // Score each requested scope against its own corpus, then merge. A node
        // lives in exactly one scope, so its entry is only ever written under
        // that scope's pass — no cross-scope key collision in `scores`.
        let mut scores: HashMap<NodeId, f32> = HashMap::new();
        for scope in scopes.iter_scopes() {
            let (n_docs, total_len) = read_stats(&stats, scope)?;
            if n_docs == 0 {
                continue;
            }
            let avgdl = total_len as f32 / n_docs as f32;
            for term in &distinct {
                let list = read_posting(&postings, posting_key(scope, term).as_slice())?;
                let df = list.len() as f32;
                if df == 0.0 {
                    continue;
                }
                let idf = ((n_docs as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
                for (id, tf) in list {
                    let len = read_doc_len(&docs, id)? as f32;
                    let tf = tf as f32;
                    let denom = tf + K1 * (1.0 - B + B * len / avgdl);
                    *scores.entry(id).or_insert(0.0) += idf * tf * (K1 + 1.0) / denom;
                }
            }
        }

        let snap = self.snapshot();
        let mut out: Vec<(NodeRecord, f32)> = scores
            .into_iter()
            .filter_map(|(id, score)| {
                snap.nodes
                    .get(&id)
                    .filter(|n| scopes.contains(n.scope))
                    .map(|n| (n.clone(), score))
            })
            .collect();
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
