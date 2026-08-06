//! Deterministic HNSW vector index (F8). One graph per (model, scope)
//! cluster, keyed beside VECTORS. All construction happens inside
//! `apply_op` (op order = insertion order); levels are an integer-only
//! function of NodeId; every internal tie breaks by slot ascending — so
//! `rebuild_state_from_ops` reproduces these tables exactly.
use crate::codec::{frame_value, unframe_value};
use crate::error::{storage_err, TopoError};
use crate::ids::NodeId;
use crate::slots::node_ulid;
use crate::vector_store::{cosine, read_vector_by_slot, vector_prefix, OrderedScore};
use redb::{ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::rc::Rc;

pub(crate) const HNSW_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_meta");
pub(crate) const HNSW_LINKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_links");
pub(crate) const HNSW_META_FORMAT_V0: u8 = 0;

/// Tuning knobs for a database's HNSW graph maintenance (F8). Threaded from
/// [`crate::db::DbOptions::hnsw_params`] (`None` resolves to `default()` at
/// open — see `Storage::open_with_options`) and re-exported as
/// `topodb::HnswParams` (mirroring how `IndexSpec` is exposed) so a host can
/// tune it, and so tests can open with a tiny `build_threshold` to exercise
/// graph activation on small corpora without seeding thousands of rows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HnswParams {
    pub version: u32,
    pub m: u32,
    pub m0: u32,
    pub ef_construction: u32,
    pub level_cap: u8,
    /// Row count (see `cluster_vector_count`) an unbuilt `(model, scope)`
    /// cluster must cross before the applier calls `build_cluster`. Two
    /// traps worth knowing: a cluster whose vectors are ALL zero-norm never
    /// actually builds (`insert` skips zero-norm rows, see `build_cluster`),
    /// so once it's crossed the threshold it re-pays the O(cluster) count on
    /// every subsequent embed forever, since it can never reach "built" and
    /// trip the count short-circuit; setting this to `u64::MAX` as a
    /// deliberate opt-out has the identical cost — the cluster can never
    /// cross it either, so it also re-pays O(cluster) per embed for as long
    /// as the opt-out stands.
    pub build_threshold: u64,
    pub rebuild_num: u32,
    pub rebuild_den: u32,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            // v2: heuristic neighbor selection (keep-pruned-connections)
            // replaced closest-M — a write-side structure change, so v1
            // graphs drain + rebuild on open via `ensure_hnsw_params`.
            version: 2,
            m: 16,
            m0: 32,
            ef_construction: 128,
            level_cap: 16,
            build_threshold: 1024,
            rebuild_num: 3,
            rebuild_den: 10,
        }
    }
}

impl HnswParams {
    pub(crate) fn validate(&self) -> Result<(), TopoError> {
        if self.m < 2 || !self.m.is_power_of_two() {
            return Err(TopoError::Rejected(format!(
                "hnsw m must be a power of two >= 2, got {}",
                self.m
            )));
        }
        if self.m0 < self.m {
            return Err(TopoError::Rejected("hnsw m0 must be >= m".into()));
        }
        if self.ef_construction < self.m {
            return Err(TopoError::Rejected(
                "hnsw ef_construction must be >= m".into(),
            ));
        }
        if self.rebuild_den == 0 || self.rebuild_num >= self.rebuild_den {
            return Err(TopoError::Rejected(
                "hnsw rebuild ratio must be a proper fraction".into(),
            ));
        }
        if self.build_threshold < 2 {
            return Err(TopoError::Rejected(
                "hnsw build_threshold must be >= 2".into(),
            ));
        }
        Ok(())
    }
}

/// One `(model, scope)` cluster's graph header. Field semantics (pinned here
/// so the ratio check in `storage.rs`'s applier wiring and any future reader
/// share one definition):
///
/// - `graph_len`: the count of slots this cluster has EVER graph-inserted
///   with a non-zero-norm vector (i.e. every `insert` call that took the
///   "real" path, not the zero-norm no-op). Monotonically non-decreasing —
///   it is NEVER decremented on `tombstone` (a tombstoned slot's row stays
///   counted; only `build_cluster` resets it, by dropping the meta row
///   entirely and re-deriving from a fresh scan of currently-live `VECTORS`
///   rows). So `graph_len` is "ever inserted, including currently
///   tombstoned" — not "currently live".
/// - `stale`: the count of rewire-worthy events since the last rebuild —
///   incremented once per `tombstone` call that newly tombstoned a slot, and
///   once per `reinsert_links` call (a same-cluster re-embed's rewire).
///   Reset to `0` only by `build_cluster` (via the dropped-then-recreated
///   meta row). The applier's ratio check compares `stale` against
///   `graph_len` (`stale/graph_len >= rebuild_num/rebuild_den`) to decide
///   when a cluster's link structure has accumulated enough dead weight to
///   rebuild from scratch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ClusterMeta {
    pub format: u8,
    pub built: bool,
    pub entry_slot: u64,
    pub entry_level: u8,
    pub graph_len: u64,
    pub stale: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LinkRow {
    pub tomb: bool,
    pub neighbors: Vec<u64>,
}

pub(crate) fn meta_key(model: u32, scope: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[..4].copy_from_slice(&model.to_be_bytes());
    k[4..].copy_from_slice(&scope.to_be_bytes());
    k
}

pub(crate) fn link_prefix(model: u32, scope: u32) -> [u8; 8] {
    meta_key(model, scope)
}

pub(crate) fn link_key(model: u32, scope: u32, slot: u64, level: u8) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[..8].copy_from_slice(&meta_key(model, scope));
    k[8..16].copy_from_slice(&slot.to_be_bytes());
    k[16] = level;
    k
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Integer-geometric level: P(level >= l) = (1/m)^l, computed from the
/// leading zeros of a splitmix64 hash of the NodeId — no RNG state, no
/// libm, bit-identical on every platform. Requires m to be a power of two
/// (validated in HnswParams::validate).
pub(crate) fn level_for(id: NodeId, m: u32, level_cap: u8) -> u8 {
    let v = id.as_u128();
    let h = splitmix64(splitmix64((v >> 64) as u64) ^ (v as u64));
    let bits_per_level = m.trailing_zeros(); // m = 2^bits
    let level = (h.leading_zeros() / bits_per_level) as u8;
    level.min(level_cap)
}

pub(crate) fn read_meta(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
) -> Result<Option<ClusterMeta>, TopoError> {
    let key = meta_key(model, scope);
    match table.get(key.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let bytes = value.value();
            let meta: ClusterMeta =
                postcard::from_bytes(bytes).map_err(|e| TopoError::Encoding(e.to_string()))?;
            if meta.format != HNSW_META_FORMAT_V0 {
                return Err(TopoError::Encoding(format!(
                    "unknown hnsw meta format 0x{:02X}",
                    meta.format
                )));
            }
            Ok(Some(meta))
        }
    }
}

pub(crate) fn write_meta(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    meta: &ClusterMeta,
) -> Result<(), TopoError> {
    let key = meta_key(model, scope);
    let bytes = postcard::to_allocvec(meta).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table
        .insert(key.as_slice(), bytes.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

pub(crate) fn read_links(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
    level: u8,
) -> Result<Option<LinkRow>, TopoError> {
    let key = link_key(model, scope, slot, level);
    match table.get(key.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let raw = unframe_value(value.value())?;
            let row: LinkRow =
                postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(row))
        }
    }
}

pub(crate) fn write_links(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
    level: u8,
    row: &LinkRow,
) -> Result<(), TopoError> {
    let key = link_key(model, scope, slot, level);
    let raw = postcard::to_allocvec(row).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let framed = frame_value(raw);
    table
        .insert(key.as_slice(), framed.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

/// Everything the algorithms need to read vectors + links. Both live in the
/// caller's transaction (write tx during apply, read tx during search).
/// Generic over the concrete table types rather than `&dyn ReadableTable`:
/// `ReadableTable::get`/`range` are generic methods (`impl Borrow<..>` /
/// `RangeBounds<..>`), which makes the trait not object-safe — `dyn
/// ReadableTable<..>` cannot be constructed. The generic-struct fallback the
/// brief names is used instead; callers monomorphize over whatever concrete
/// `redb::Table`/`redb::ReadOnlyTable` they have open.
pub(crate) struct GraphReader<'a, V, R>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    pub vectors: &'a V,
    pub refs: &'a R,
    pub model: u32,
    pub scope: u32,
}

/// Per-graph-op decoded-vector memo over `read_vector_by_slot` + the
/// cluster check. Profiling the 20k×384 build put ~40% of insert time in
/// vector fetch (redb b-tree navigation + postcard `Vec<f32>` decode), and
/// one insert asks for the same slot's vector repeatedly: once per visited
/// slot per level in `search_layer`, again in `select_neighbors`'s
/// resolver, and again inside every `prune_neighbor` call (whose candidate
/// sets overlap heavily). One cache instance lives for ONE graph op
/// (`insert` / `reinsert_links` / one `search` query) — never across ops,
/// so a re-embed in a later op can't serve a stale hit, and memory stays
/// bounded by the op's visited set. Within an op the tables are borrowed
/// for its whole duration, so point-in-time stability is the already-
/// guaranteed semantics — the memo changes no value the graph computes,
/// only how often it is decoded. Negative results (absent slot, wrong
/// cluster) are cached too: `prune_neighbor` re-asks about the same
/// unresolvable candidate across calls. The map is lookup-only (never
/// iterated), per the module's determinism rules; `Rc` because one op is
/// strictly single-threaded (the applier).
pub(crate) struct VecCache {
    map: HashMap<u64, Option<Rc<Vec<f32>>>>,
}

impl VecCache {
    pub(crate) fn new() -> Self {
        VecCache {
            map: HashMap::new(),
        }
    }

    pub(crate) fn get<V, R>(
        &mut self,
        reader: &GraphReader<'_, V, R>,
        slot: u64,
    ) -> Result<Option<Rc<Vec<f32>>>, TopoError>
    where
        V: ReadableTable<&'static [u8], &'static [u8]>,
        R: ReadableTable<&'static [u8], &'static [u8]>,
    {
        if let Some(hit) = self.map.get(&slot) {
            return Ok(hit.clone());
        }
        let resolved = match read_vector_by_slot(reader.vectors, reader.refs, slot)? {
            Some((m, s, v)) if m == reader.model && s == reader.scope => Some(Rc::new(v)),
            _ => None,
        };
        self.map.insert(slot, resolved.clone());
        Ok(resolved)
    }
}

/// The one greedy routine both `insert` and `search` use. Seeds candidates +
/// results with `entry_pts`, then repeatedly pops the best (highest-cosine)
/// candidate and expands its `LinkRow.neighbors` at `level`, until the best
/// remaining candidate is strictly worse than the worst kept result and the
/// result set is already at `ef`. Tombstoned slots are never pushed into
/// `results` (so they're never returned) but ARE pushed into `candidates`
/// (so their neighbor lists still get explored) — "tombs route but don't
/// rank." Every tie (candidate pop order, result eviction order, final
/// output order) breaks on slot ascending; the only `HashSet` here
/// (`visited`) is used strictly for membership tests, never iterated.
fn search_layer<V, R>(
    links: &impl ReadableTable<&'static [u8], &'static [u8]>,
    reader: &GraphReader<'_, V, R>,
    cache: &mut VecCache,
    entry_pts: &[u64],
    query: &[f32],
    ef: usize,
    level: u8,
) -> Result<Vec<(OrderedScore, u64)>, TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    let mut visited: HashSet<u64> = HashSet::new();
    // Candidate max-heap: BinaryHeap pops the greatest element first, so the
    // natural `OrderedScore` order pops the closest (highest-cosine)
    // candidate first; on a tie, `Reverse<u64>` makes the LOWER slot compare
    // greater (Reverse(1) > Reverse(2)), so it pops before the higher slot.
    let mut candidates: BinaryHeap<(OrderedScore, Reverse<u64>)> = BinaryHeap::new();
    // Result bound-heap: `Reverse` of the same tuple turns the max-heap into
    // a min-heap over `(score, slot)`, so `peek`/`pop` surface the WORST kept
    // result — the one to evict once `results.len() > ef`.
    let mut results: BinaryHeap<Reverse<(OrderedScore, Reverse<u64>)>> = BinaryHeap::new();

    let seed = |visited: &mut HashSet<u64>,
                candidates: &mut BinaryHeap<(OrderedScore, Reverse<u64>)>,
                results: &mut BinaryHeap<Reverse<(OrderedScore, Reverse<u64>)>>,
                cache: &mut VecCache,
                slot: u64|
     -> Result<(), TopoError> {
        if !visited.insert(slot) {
            return Ok(());
        }
        // A missing link row at this level means `slot` was never a graph
        // member here (or this level's structure was since rebuilt away) —
        // there is nothing to route through or rank.
        let Some(row) = read_links(links, reader.model, reader.scope, slot, level)? else {
            return Ok(());
        };
        // Resolve a live, in-cluster, non-zero-norm vector to score `slot`
        // by. `None` covers three cases that all still need `slot` ROUTED
        // through (its link row above proves it's a real graph member) even
        // though it can never be RANKED: the node was fully removed
        // (`RemoveNode`'s `remove_vector` deletes the `VECTORS`/
        // `EMBEDDING_REF` rows entirely, unlike `tombstone` which only flips
        // a flag — so a tombstoned slot's vector is gone by the time a
        // search walks through it), it moved to a different cluster (a
        // cross-model re-embed's `put_vector` deletes the OLD cluster's
        // `VECTORS` row the same way), or its embedding is zero-norm
        // (`cosine` returns `None`, mirroring `insert`'s zero-norm no-op).
        // Resolution goes through the op's `VecCache` — same value, decoded
        // at most once per op.
        let scoreable = match cache.get(reader, slot)? {
            Some(v) => cosine(query, &v),
            None => None,
        };
        match scoreable {
            Some(score) => {
                let os = OrderedScore(score);
                candidates.push((os, Reverse(slot)));
                if !row.tomb {
                    results.push(Reverse((os, Reverse(slot))));
                    if results.len() > ef.max(1) {
                        results.pop();
                    }
                }
            }
            None => {
                // Un-scoreable but still a real graph member at this level:
                // push it into `candidates` at the HIGHEST possible priority
                // (`f32::INFINITY` compares greatest under `total_cmp`) so
                // it's expanded unconditionally, ignoring the ef/worst-score
                // early-termination heuristic below — otherwise a removed
                // (or moved-away) node would silently sever the graph's
                // connectivity through it. Never eligible for `results`:
                // there is no score to rank it by.
                candidates.push((OrderedScore(f32::INFINITY), Reverse(slot)));
            }
        }
        Ok(())
    };

    for &slot in entry_pts {
        seed(&mut visited, &mut candidates, &mut results, cache, slot)?;
    }

    while let Some(&(cand_score, Reverse(cand_slot))) = candidates.peek() {
        if results.len() >= ef {
            if let Some(&Reverse((worst_score, _))) = results.peek() {
                if cand_score < worst_score {
                    break;
                }
            }
        }
        candidates.pop();
        let Some(row) = read_links(links, reader.model, reader.scope, cand_slot, level)? else {
            continue;
        };
        for &nbr_slot in &row.neighbors {
            seed(&mut visited, &mut candidates, &mut results, cache, nbr_slot)?;
        }
    }

    let mut out: Vec<(OrderedScore, u64)> = results
        .into_iter()
        .map(|Reverse((score, Reverse(slot)))| (score, slot))
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    if out.len() > ef {
        out.truncate(ef);
    }
    Ok(out)
}

/// Heuristic neighbor selection with keep-pruned-connections (Malkov &
/// Yashunin Alg. 4, similarity-space form) — the shared policy behind
/// `insert`, `reinsert_links`, and `prune_neighbor`. `candidates` is
/// `(similarity-to-query, slot)` in `search_layer`'s output order
/// (score desc, slot asc); a candidate is KEPT only when it is strictly
/// closer to the query than to every already-kept neighbor
/// (`sim(e, query) > sim(e, kept)` — an exact tie prunes), which stops a
/// mutually-close clump from monopolizing the row the way plain closest-M
/// does. Pruned candidates backfill remaining budget in candidate order
/// (keep-pruned-connections), so a row is short only when the candidate
/// list itself is. The returned row preserves candidate order.
///
/// Determinism: the walk order is the caller's sorted order, every
/// comparison is `total_cmp` over the same `cosine` the graph is built
/// from, and the one `HashSet` is membership-only (never iterated). A
/// slot `resolve`ing to `None` (removed / moved cluster mid-txn) is
/// skipped entirely — never kept, never backfilled; an unscoreable
/// kept-pair (`cosine` `None`) cannot block a candidate.
fn select_neighbors<F>(
    candidates: &[(OrderedScore, u64)],
    max_m: usize,
    mut resolve: F,
) -> Result<Vec<u64>, TopoError>
where
    F: FnMut(u64) -> Result<Option<Rc<Vec<f32>>>, TopoError>,
{
    let mut kept: Vec<(u64, Rc<Vec<f32>>)> = Vec::with_capacity(max_m.min(candidates.len()));
    let mut pruned: Vec<u64> = Vec::new();
    for &(score, slot) in candidates {
        if kept.len() >= max_m {
            break;
        }
        let Some(v) = resolve(slot)? else {
            continue;
        };
        let diverse = kept.iter().all(|(_, kv)| match cosine(&v, kv) {
            Some(sim_to_kept) => OrderedScore(sim_to_kept) < score,
            None => true,
        });
        if diverse {
            kept.push((slot, v));
        } else {
            pruned.push(slot);
        }
    }
    let mut members: HashSet<u64> = kept.iter().map(|&(slot, _)| slot).collect();
    for slot in pruned {
        if members.len() >= max_m {
            break;
        }
        members.insert(slot);
    }
    Ok(candidates
        .iter()
        .map(|&(_, slot)| slot)
        .filter(|slot| members.contains(slot))
        .collect())
}

/// Re-selects `neighbor_slot`'s link row at `level` after appending
/// `new_slot`: scores every one of its (possibly now `max_m + 1`) neighbors
/// by cosine FROM THE NEIGHBOR (not from the original query), closest first,
/// slot-ascending on ties, then applies `select_neighbors` down to `max_m`.
/// No-op if the row is already within budget after the append.
fn prune_neighbor<V, R>(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    reader: &GraphReader<'_, V, R>,
    cache: &mut VecCache,
    neighbor_slot: u64,
    new_slot: u64,
    level: u8,
    max_m: usize,
) -> Result<(), TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    let mut row =
        read_links(links, reader.model, reader.scope, neighbor_slot, level)?.unwrap_or(LinkRow {
            tomb: false,
            neighbors: Vec::new(),
        });
    if !row.neighbors.contains(&new_slot) {
        row.neighbors.push(new_slot);
    }
    if row.neighbors.len() > max_m {
        // Mirror `search`'s `seed` closure: only a vector still resolving
        // into THIS `(reader.model, reader.scope)` cluster is trustworthy to
        // score with (the `VecCache` applies that check on every fill). A
        // cross-model-moved slot would otherwise score retained neighbors
        // by a foreign vector space; it resolves to `None`, same as a fully
        // removed slot.
        match cache.get(reader, neighbor_slot)? {
            Some(nv) => {
                let mut scored: Vec<(OrderedScore, u64)> = Vec::with_capacity(row.neighbors.len());
                for &cand in &row.neighbors {
                    // A cross-model-moved (or fully removed) candidate is
                    // unscoreable and, unlike `search`'s seed closure (which
                    // must still ROUTE through an unscoreable member to
                    // preserve connectivity), prune has no routing duty here
                    // — it is purely selecting which neighbors to KEEP, so a
                    // mismatch is simply dropped from the retained set.
                    if let Some(cv) = cache.get(reader, cand)? {
                        if let Some(score) = cosine(&nv, &cv) {
                            scored.push((OrderedScore(score), cand));
                        }
                    }
                }
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                // Same heuristic as `insert`, from the NEIGHBOR's own
                // perspective (`nv` is the "query"); every resolve is a
                // cache hit — the scoring loop above just filled them.
                row.neighbors = select_neighbors(&scored, max_m, |s| cache.get(reader, s))?;
            }
            None => {
                // Defensive fallback only: `neighbor_slot` was reached
                // through a link row that is written under `put_vector`'s
                // invariant that a live slot always resolves, so this arm is
                // unreached in every test; kept deterministic (slot
                // ascending) rather than panicking should the invariant ever
                // be violated.
                row.neighbors.sort_unstable();
                row.neighbors.truncate(max_m);
            }
        }
    }
    write_links(
        links,
        reader.model,
        reader.scope,
        neighbor_slot,
        level,
        &row,
    )
}

/// Inserts `slot` (with pre-resolved `vector`) into the `(reader.model,
/// reader.scope)` graph. `id` is used ONLY for `level_for` — never for
/// ordering. A zero-norm vector (`cosine(vector, vector) == None`) is a
/// deliberate no-op: it can never usefully route or be routed to, so it must
/// never touch `links`/`meta`. First node in an (absent-meta) cluster
/// bootstraps `meta` directly; every later node greedily descends from the
/// current entry point (ef=1) down to `level + 1`, then does a full
/// `ef_construction` search at each level from `min(level, entry_level)`
/// down to 0, wiring itself to `M`/`M0` (`M0` at level 0) neighbors chosen
/// by `select_neighbors` (the diversity heuristic, not plain closest-M) and
/// pruning each of those neighbors back down to its own budget.
pub(crate) fn insert<V, R>(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    meta: &mut Table<'_, &'static [u8], &'static [u8]>,
    reader: &GraphReader<'_, V, R>,
    params: &HnswParams,
    slot: u64,
    id: NodeId,
    vector: &[f32],
) -> Result<(), TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    if cosine(vector, vector).is_none() {
        return Ok(()); // zero-norm: never enters the graph.
    }

    // One decoded-vector memo for this whole insert: descent, per-level
    // construction searches, selection, and every prune share it.
    let mut cache = VecCache::new();
    let level = level_for(id, params.m, params.level_cap);

    let cur_meta = match read_meta(meta, reader.model, reader.scope)? {
        None => {
            for lvl in 0..=level {
                write_links(
                    links,
                    reader.model,
                    reader.scope,
                    slot,
                    lvl,
                    &LinkRow {
                        tomb: false,
                        neighbors: Vec::new(),
                    },
                )?;
            }
            write_meta(
                meta,
                reader.model,
                reader.scope,
                &ClusterMeta {
                    format: HNSW_META_FORMAT_V0,
                    built: true,
                    entry_slot: slot,
                    entry_level: level,
                    graph_len: 1,
                    stale: 0,
                },
            )?;
            return Ok(());
        }
        Some(m) => m,
    };

    let entry_level = cur_meta.entry_level;
    let mut entry_slot = cur_meta.entry_slot;

    // Greedy descend ef=1 from the current entry level down to `level + 1`
    // — only refines which single node we start the real construction
    // search from; never touches links.
    let mut descend_level = entry_level;
    while descend_level > level {
        let hits = search_layer(
            links,
            reader,
            &mut cache,
            &[entry_slot],
            vector,
            1,
            descend_level,
        )?;
        if let Some(&(_, best)) = hits.first() {
            entry_slot = best;
        }
        // The `while` guard keeps descend_level >= 1 inside the body (level
        // is unsigned), so this decrement can never underflow.
        descend_level -= 1;
    }

    // Levels strictly above `entry_level` (only when this node's level beats
    // the current entry) have no existing structure to connect to yet.
    if level > entry_level {
        for lvl in (entry_level + 1)..=level {
            write_links(
                links,
                reader.model,
                reader.scope,
                slot,
                lvl,
                &LinkRow {
                    tomb: false,
                    neighbors: Vec::new(),
                },
            )?;
        }
    }

    let top = level.min(entry_level);
    let mut entry_pts = vec![entry_slot];
    let mut cur_level = top;
    loop {
        let ef_c = params.ef_construction as usize;
        let candidates = search_layer(
            links, reader, &mut cache, &entry_pts, vector, ef_c, cur_level,
        )?;
        let max_m = if cur_level == 0 { params.m0 } else { params.m } as usize;
        let selected = select_neighbors(&candidates, max_m, |s| cache.get(reader, s))?;

        write_links(
            links,
            reader.model,
            reader.scope,
            slot,
            cur_level,
            &LinkRow {
                tomb: false,
                neighbors: selected.clone(),
            },
        )?;
        for &nbr in &selected {
            prune_neighbor(links, reader, &mut cache, nbr, slot, cur_level, max_m)?;
        }

        entry_pts = candidates.into_iter().map(|(_, s)| s).collect();
        if entry_pts.is_empty() {
            entry_pts = vec![entry_slot];
        }

        if cur_level == 0 {
            break;
        }
        cur_level -= 1;
    }

    let mut new_meta = cur_meta;
    new_meta.graph_len += 1;
    if level > entry_level {
        new_meta.entry_slot = slot;
        new_meta.entry_level = level;
    }
    write_meta(meta, reader.model, reader.scope, &new_meta)?;
    Ok(())
}

/// Rewires `slot`'s OWN out-links after a same-cluster re-embed (same
/// `(model, scope)` as before — a genuinely different cluster is a
/// tombstone-in-the-old-cluster-plus-fresh-`insert`-in-the-new-one,
/// handled by the caller, not this function). Unlike `insert`, this never
/// touches any OTHER slot's `LinkRow`: no `prune_neighbor` back-linking, no
/// neighbor-side budget enforcement — only `slot`'s own row at each level it
/// already occupies gets a fresh `ef_construction` search and an overwrite.
///
/// No-op (no reads even attempted beyond the initial `read_meta`/
/// `read_links` checks) when:
/// - the cluster has no `meta` row yet (unbuilt: nothing to rewire), or
/// - `slot` has no existing level-0 `HNSW_LINKS` row (it was never actually
///   graph-inserted — e.g. its PRIOR embedding was zero-norm and `insert`
///   no-op'd on it — so this is really a fresh-insert case; the caller is
///   expected to have already ruled this out via the same check before
///   calling, but this function re-checks defensively rather than silently
///   fabricating a partial row).
///
/// A zero-norm `vector` (the NEW embedding) is symmetric with `insert`'s
/// zero-norm rule but resolves the opposite way: since the slot IS already a
/// live graph member, it can't simply be skipped — it must stop routing/
/// ranking, so this delegates to `tombstone` instead of writing degenerate
/// links.
///
/// Every level `0..=own_level` gets rewired, `own_level` being the highest
/// level `slot` already occupies (discovered by probing `HNSW_LINKS`
/// upward from level 0 until the first missing row — exactly the
/// contiguous `0..=level` range `insert` originally wrote). At each level,
/// `slot` is filtered out of the `search_layer` results before they're used
/// as the new neighbor list or as next-level `entry_pts`: `slot` already has
/// rows at every level `insert` would have written, so a naive search can
/// route straight back to (and even seed on) `slot` itself — most acutely
/// when `slot` IS the cluster's `entry_slot`, where the very first seed call
/// would otherwise self-score a trivial `1.0` and short-circuit the search.
/// Always sets the rewritten row's `tomb` to `false` (a live re-embed is
/// definitionally live) and bumps `meta.stale` by exactly 1 — a rewire is as
/// stale-inducing as a tombstone for the module's rebuild-ratio accounting
/// (see `ClusterMeta`'s field doc comment).
pub(crate) fn reinsert_links<V, R>(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    meta: &mut Table<'_, &'static [u8], &'static [u8]>,
    reader: &GraphReader<'_, V, R>,
    params: &HnswParams,
    slot: u64,
    vector: &[f32],
) -> Result<(), TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    let Some(cur_meta) = read_meta(meta, reader.model, reader.scope)? else {
        return Ok(()); // unbuilt cluster: nothing to rewire.
    };
    if read_links(links, reader.model, reader.scope, slot, 0)?.is_none() {
        return Ok(()); // never actually graph-inserted: caller should `insert` instead.
    }

    if cosine(vector, vector).is_none() {
        // Zero-norm re-embed: the slot can no longer usefully route or be
        // routed to, so stop ranking it — same outcome as an explicit
        // removal, reusing `tombstone` rather than duplicating its logic.
        tombstone(links, meta, reader.model, reader.scope, slot)?;
        return Ok(());
    }

    // Discover the contiguous level range `insert` originally wrote for this
    // slot (level 0 is already confirmed present above).
    let mut own_level: u8 = 0;
    while read_links(links, reader.model, reader.scope, slot, own_level + 1)?.is_some() {
        own_level += 1;
    }

    let entry_level = cur_meta.entry_level;
    let mut entry_slot = cur_meta.entry_slot;
    // `own_level <= entry_level` always: `level_for` is a pure function of
    // NodeId, so this slot's level never changes, and `entry_level` only
    // ever grows to match the highest level any inserted node has reached —
    // it was already >= `own_level` back when this slot was first inserted.
    let level = own_level.min(entry_level);

    // One memo for the whole rewire, mirroring `insert`.
    let mut cache = VecCache::new();
    let mut descend_level = entry_level;
    while descend_level > level {
        let hits = search_layer(
            links,
            reader,
            &mut cache,
            &[entry_slot],
            vector,
            1,
            descend_level,
        )?;
        if let Some(&(_, best)) = hits.iter().find(|&&(_, s)| s != slot) {
            entry_slot = best;
        }
        // The `while` guard keeps descend_level >= 1 inside the body (level
        // is unsigned), so this decrement can never underflow.
        descend_level -= 1;
    }

    let mut entry_pts = vec![entry_slot];
    let mut cur_level = level;
    loop {
        let ef_c = params.ef_construction as usize;
        let candidates = search_layer(
            links, reader, &mut cache, &entry_pts, vector, ef_c, cur_level,
        )?;
        let filtered: Vec<(OrderedScore, u64)> =
            candidates.into_iter().filter(|&(_, s)| s != slot).collect();
        let max_m = if cur_level == 0 { params.m0 } else { params.m } as usize;
        let selected = select_neighbors(&filtered, max_m, |s| cache.get(reader, s))?;

        write_links(
            links,
            reader.model,
            reader.scope,
            slot,
            cur_level,
            &LinkRow {
                tomb: false,
                neighbors: selected,
            },
        )?;

        entry_pts = filtered.into_iter().map(|(_, s)| s).collect();
        if entry_pts.is_empty() {
            entry_pts = vec![entry_slot];
        }

        if cur_level == 0 {
            break;
        }
        cur_level -= 1;
    }

    let mut new_meta = cur_meta;
    new_meta.stale += 1;
    write_meta(meta, reader.model, reader.scope, &new_meta)?;
    Ok(())
}

/// Marks `slot`'s level-0 link row as tombstoned — `Ok(false)` if the slot
/// has no level-0 row (never inserted, or already removed from the graph
/// some other way) or is already tombstoned, `Ok(true)` if this call is the
/// one that newly tombstoned it. Only flips the flag and bumps `meta.stale`;
/// the row's `neighbors` (routing structure) are left exactly as they were,
/// per the module's "tombs route but don't rank" contract.
pub(crate) fn tombstone(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    meta: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
) -> Result<bool, TopoError> {
    let Some(mut row) = read_links(links, model, scope, slot, 0)? else {
        return Ok(false);
    };
    if row.tomb {
        return Ok(false);
    }
    row.tomb = true;
    write_links(links, model, scope, slot, 0, &row)?;
    if let Some(mut m) = read_meta(meta, model, scope)? {
        m.stale += 1;
        write_meta(meta, model, scope, &m)?;
    }
    Ok(true)
}

/// The `ef` a read-path search uses for a built cluster's level-0 pass:
/// `max(4*k, 64)` — wide enough to give small-`k` queries real recall
/// headroom over the greedy descent while staying bounded regardless of how
/// large `k` gets. `search`'s own `ef.max(k)` clamp still applies on top of
/// this, so `ef_search(k) >= k` always holds trivially.
pub(crate) fn ef_search(k: usize) -> usize {
    (4 * k).max(64)
}

/// Greedy-descends from `meta_row`'s entry point (ef=1 per level down to 1),
/// then runs one `search_layer` at level 0 with `ef = max(ef, k)`, returning
/// up to that many non-tombstoned `(slot, exact cosine)` pairs sorted
/// `(score desc, slot asc)`. Callers resolve `NodeId`s and apply any further
/// `(score desc, NodeId asc)` re-sort themselves (`vector.rs`, as today).
///
/// A zero-norm `query` yields an EMPTY result by design, not an error:
/// `search_layer`'s `seed` closure drops every candidate whose `cosine`
/// score is `None` (the module-wide zero-norm-skip rule shared with
/// `insert`), and `cosine(query, _)` is `None` for every candidate when
/// `query` itself has zero norm — so both the greedy descend and the final
/// level-0 search visit no candidate and `results` stays empty all the way
/// through. See `zero_norm_query_yields_empty_result` below for the pinned
/// case.
pub(crate) fn search<V, R>(
    links: &impl ReadableTable<&'static [u8], &'static [u8]>,
    meta_row: &ClusterMeta,
    reader: &GraphReader<'_, V, R>,
    query: &[f32],
    ef: usize,
    k: usize,
) -> Result<Vec<(u64, f32)>, TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
{
    let ef_eff = ef.max(k);
    // Per-query memo: the descent and the level-0 sweep revisit slots
    // (the entry region especially), and each revisit is a full b-tree
    // get + postcard decode without it.
    let mut cache = VecCache::new();
    let mut entry_slot = meta_row.entry_slot;
    let mut cur_level = meta_row.entry_level;
    while cur_level > 0 {
        let hits = search_layer(
            links,
            reader,
            &mut cache,
            &[entry_slot],
            query,
            1,
            cur_level,
        )?;
        if let Some(&(_, best)) = hits.first() {
            entry_slot = best;
        }
        cur_level -= 1;
    }
    let hits = search_layer(links, reader, &mut cache, &[entry_slot], query, ef_eff, 0)?;
    Ok(hits
        .into_iter()
        .map(|(score, slot)| (slot, score.0))
        .collect())
}

/// The number of `VECTORS` rows currently stored under `(model, scope)` —
/// used by the applier to decide whether an as-yet-unbuilt cluster has just
/// crossed `HnswParams::build_threshold`. A plain range-count over
/// `vector_prefix(model, scope)`, so it's O(cluster size) per call; that's
/// only ever paid PRE-build, and only below the threshold (once a cluster is
/// built, the applier calls `insert` directly instead of re-counting), so it
/// stays cheap for every threshold this module ships with (default 1024).
/// Counts EVERY row in the cluster, including zero-norm ones that will never
/// actually enter the graph via `insert` — the threshold is about "how big
/// has this cluster's key space gotten", not "how many nodes would `insert`
/// accept", mirroring `build_cluster`'s own scan (which also visits
/// zero-norm rows before skipping them).
pub(crate) fn cluster_vector_count(
    vectors: &impl ReadableTable<&'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
) -> Result<u64, TopoError> {
    let prefix = vector_prefix(model, scope);
    let mut start = prefix.to_vec();
    start.extend_from_slice(&0u64.to_be_bytes());
    let mut end = prefix.to_vec();
    end.extend_from_slice(&u64::MAX.to_be_bytes());
    let mut count = 0u64;
    for entry in vectors
        .range(start.as_slice()..=end.as_slice())
        .map_err(storage_err)?
    {
        entry.map_err(storage_err)?;
        count += 1;
    }
    Ok(count)
}

/// Every distinct `(model, scope)` pair with at least one row in `VECTORS`,
/// in ascending key order (`vector_key`'s `(model, scope)` prefix sorts
/// first, so every row belonging to one cluster is contiguous). A single
/// forward scan of the whole table, deduping consecutive rows that share the
/// same 8-byte prefix — `VECTORS` has no secondary index over just the
/// prefix, so this is the only way to enumerate clusters without already
/// knowing which `(model, scope)` pairs exist. Used by `Storage::
/// ensure_hnsw_params` after an `hnsw_params` mismatch has already drained
/// both HNSW tables, to decide which clusters are eligible to rebuild — paid
/// only on that reconcile path, never on an ordinary open.
pub(crate) fn clusters(
    vectors: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<Vec<(u32, u32)>, TopoError> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for entry in vectors.iter().map_err(storage_err)? {
        let (key_guard, _) = entry.map_err(storage_err)?;
        let key = key_guard.value();
        let model = u32::from_be_bytes(
            key[0..4]
                .try_into()
                .map_err(|_| TopoError::Encoding("bad vector_key length".into()))?,
        );
        let scope = u32::from_be_bytes(
            key[4..8]
                .try_into()
                .map_err(|_| TopoError::Encoding("bad vector_key length".into()))?,
        );
        if out.last() != Some(&(model, scope)) {
            out.push((model, scope));
        }
    }
    Ok(out)
}

/// Rebuilds the `(model, scope)` graph from scratch: deletes every
/// `HNSW_LINKS` row under `link_prefix(model, scope)`, drops the cluster's
/// `meta` row entirely (not merely resets its fields — an ABSENT meta row is
/// what makes the first `insert` call below take the "first node" bootstrap
/// branch, so build is bit-for-bit "insert every live vector, in slot
/// order, into an empty graph" — the exact seam `build_cluster_is_
/// equivalent_to_incremental_inserts` pins), then walks `VECTORS` over
/// `vector_prefix(model, scope)` in key order (slot ascending, since the key
/// is `(model, scope, slot)` big-endian), resolving each slot's `NodeId` via
/// `node_ulid` for `level_for` and re-inserting it. Zero-norm rows are
/// skipped before even resolving their `NodeId` (an equivalent, cheaper
/// no-op to letting `insert` reject them).
///
/// **Memory bound.** The `VECTORS` walk is STREAMED (decode one row, insert
/// it, drop it, advance) rather than collected into a `Vec<(u64, Vec<f32>)>`
/// up front — the stale-link-key collect above still buffers (17 bytes/key,
/// negligible even at scale) since deletion needs the full key set before
/// any `remove` call, but the far larger per-row `Vec<f32>` embeddings never
/// are. Residual memory for this function is O(1) in cluster size: one
/// decoded vector alive at a time plus `insert`'s own O(M * ef_construction)
/// working set — not O(cluster size), which the old buffered form was
/// (~1.5 GB transient at 1M rows x 384 dims). See BENCHMARKS.md's v7 section
/// for what the gate runner should watch as a result.
#[allow(clippy::too_many_arguments)] // exact signature pinned by the f8 task brief
pub(crate) fn build_cluster<V, R, VI, NI>(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    meta: &mut Table<'_, &'static [u8], &'static [u8]>,
    vectors_iter_source: &VI,
    reader: &GraphReader<'_, V, R>,
    node_ids: &NI,
    params: &HnswParams,
    model: u32,
    scope: u32,
) -> Result<(), TopoError>
where
    V: ReadableTable<&'static [u8], &'static [u8]>,
    R: ReadableTable<&'static [u8], &'static [u8]>,
    VI: ReadableTable<&'static [u8], &'static [u8]>,
    NI: ReadableTable<&'static [u8], &'static [u8]>,
{
    let prefix = link_prefix(model, scope);
    let mut start = prefix.to_vec();
    start.extend_from_slice(&[0u8; 9]);
    let mut end = prefix.to_vec();
    end.extend_from_slice(&[0xFFu8; 9]);
    let mut stale_keys: Vec<Vec<u8>> = Vec::new();
    for entry in links
        .range(start.as_slice()..=end.as_slice())
        .map_err(storage_err)?
    {
        let (k, _v) = entry.map_err(storage_err)?;
        stale_keys.push(k.value().to_vec());
    }
    for k in stale_keys {
        links.remove(k.as_slice()).map_err(storage_err)?;
    }

    meta.remove(meta_key(model, scope).as_slice())
        .map_err(storage_err)?;

    let vprefix = vector_prefix(model, scope);
    let mut vstart = vprefix.to_vec();
    vstart.extend_from_slice(&0u64.to_be_bytes());
    let mut vend = vprefix.to_vec();
    vend.extend_from_slice(&u64::MAX.to_be_bytes());
    // Streamed, not buffered: `VECTORS` is never mutated by this function (or
    // by `insert`, which only touches `links`/`meta`), so `vectors_iter_
    // source`'s range iterator and `reader.vectors` — the SAME underlying
    // table, passed in twice as two shared `&_` borrows by every caller — can
    // be held live simultaneously. Each row's `(slot, vector)` is decoded,
    // immediately inserted, and dropped before the next `.next()` call, so
    // residual memory is O(1) in cluster size: one decoded vector plus
    // `insert`'s own O(M * ef_construction) working set, never the whole
    // cluster's `Vec<(u64, Vec<f32>)>` (this replaced an ~1.5 GB transient
    // buffer at 1M rows x 384 dims — see this fn's doc comment and
    // BENCHMARKS.md).
    for entry in vectors_iter_source
        .range(vstart.as_slice()..=vend.as_slice())
        .map_err(storage_err)?
    {
        let (key_guard, value_guard) = entry.map_err(storage_err)?;
        let key = key_guard.value();
        let slot_bytes: [u8; 8] = key[8..16]
            .try_into()
            .map_err(|_| TopoError::Encoding("bad vector_key length".into()))?;
        let slot = u64::from_be_bytes(slot_bytes);
        let raw = unframe_value(value_guard.value())?;
        let vector: Vec<f32> =
            postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
        drop(key_guard);
        drop(value_guard);
        if cosine(&vector, &vector).is_none() {
            continue; // zero-norm: skip before even resolving a NodeId.
        }
        let Some(id) = node_ulid(node_ids, slot)? else {
            continue; // no ULID mapping for this slot: cannot compute level_for.
        };
        insert(links, meta, reader, params, slot, id, &vector)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeId;

    #[test]
    fn level_distribution_and_pins() {
        // Integer-geometric: P(level >= l) = 16^-l. Pin exact values for
        // fixed ids so any change to the hash or formula is loud.
        let l0 = level_for(NodeId::from_u128(10), 16, 16);
        let l1 = level_for(NodeId::from_u128(11), 16, 16);
        // Deterministic: same input, same output, always.
        assert_eq!(l0, level_for(NodeId::from_u128(10), 16, 16));
        assert_eq!(l1, level_for(NodeId::from_u128(11), 16, 16));
        // Distribution sanity over a range: level 0 dominates ~15/16.
        let mut counts = [0usize; 17];
        for i in 0..4096u128 {
            counts[level_for(NodeId::from_u128(i), 16, 16) as usize] += 1;
        }
        assert!(
            counts[0] > 3500,
            "level 0 should be ~15/16 of 4096, got {}",
            counts[0]
        );
        assert!(
            counts[1] > 100,
            "level 1 should be ~1/16 of 4096, got {}",
            counts[1]
        );
        // Cap respected.
        for i in 0..4096u128 {
            assert!(level_for(NodeId::from_u128(i), 16, 3) <= 3);
        }
    }

    #[test]
    fn keys_are_prefix_ordered() {
        let p = link_prefix(7, 9);
        let k = link_key(7, 9, 42, 3);
        assert_eq!(&k[..8], &p[..]);
        // Slot-major then level within a cluster.
        assert!(link_key(7, 9, 1, 5) < link_key(7, 9, 2, 0));
        assert!(link_key(7, 9, 2, 0) < link_key(7, 9, 2, 1));
        assert_eq!(meta_key(7, 9), p);
    }

    #[test]
    fn ef_search_is_max_of_4k_and_64() {
        assert_eq!(ef_search(0), 64);
        assert_eq!(ef_search(1), 64);
        assert_eq!(ef_search(10), 64); // 4*10=40 < 64
        assert_eq!(ef_search(16), 64); // 4*16=64, boundary
        assert_eq!(ef_search(17), 68); // 4*17=68 > 64
        assert_eq!(ef_search(1000), 4000);
    }

    #[test]
    fn params_roundtrip_and_validate() {
        let p = HnswParams::default();
        p.validate().unwrap();
        let bytes = postcard::to_allocvec(&p).unwrap();
        assert_eq!(postcard::from_bytes::<HnswParams>(&bytes).unwrap(), p);
        assert!(
            HnswParams {
                m: 12,
                ..HnswParams::default()
            }
            .validate()
            .is_err(),
            "m must be a power of two"
        );
        assert!(HnswParams {
            rebuild_num: 10,
            rebuild_den: 10,
            ..HnswParams::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn link_row_roundtrip_via_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            write_links(
                &mut links,
                1,
                2,
                3,
                0,
                &LinkRow {
                    tomb: false,
                    neighbors: vec![5, 9, 1],
                },
            )
            .unwrap();
            write_meta(
                &mut meta,
                1,
                2,
                &ClusterMeta {
                    format: HNSW_META_FORMAT_V0,
                    built: true,
                    entry_slot: 3,
                    entry_level: 0,
                    graph_len: 1,
                    stale: 0,
                },
            )
            .unwrap();
            assert_eq!(
                read_links(&links, 1, 2, 3, 0).unwrap().unwrap().neighbors,
                vec![5, 9, 1]
            );
            assert!(
                read_links(&links, 1, 2, 4, 0).unwrap().is_none(),
                "missing key is Ok(None)"
            );
            assert_eq!(read_meta(&meta, 1, 2).unwrap().unwrap().entry_slot, 3);
        }
        tx.commit().unwrap();
    }

    // -- Task 2: insert / search / build primitives -------------------------

    use crate::slots::{alloc_node_slot, NODE_IDS, NODE_SLOTS};
    use crate::storage::META as SLOT_ALLOC_META;
    use crate::vector_store::{put_vector, EMBEDDING_REF, VECTORS};
    use redb::Database;

    /// Deterministic splitmix64 float generator — the exact idiom from
    /// `benches/storage.rs:321-331`'s `VecRng`, copied rather than shared
    /// (that struct is bench-crate-local) so this module's seeded fixtures
    /// need no RNG crate and reproduce byte-for-byte across runs.
    struct VecRng(u64);
    impl VecRng {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z as f32 / u64::MAX as f32) * 2.0 - 1.0
        }
    }

    fn seed_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut rng = VecRng(seed);
        (0..n)
            .map(|_| (0..dim).map(|_| rng.next_f32()).collect())
            .collect()
    }

    fn open_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        (dir, db)
    }

    /// Brute-force top-k over `entries` (excluding any slot in `tombstoned`),
    /// scored with the SAME `cosine` HNSW uses, ordered `(score desc via
    /// `OrderedScore`/`total_cmp`, slot asc)` — the reference oracle
    /// `insert`+`search` are checked against.
    fn brute_force(
        entries: &[(u64, Vec<f32>)],
        tombstoned: &HashSet<u64>,
        query: &[f32],
        k: usize,
    ) -> Vec<(u64, f32)> {
        let mut scored: Vec<(u64, f32)> = entries
            .iter()
            .filter(|(slot, _)| !tombstoned.contains(slot))
            .filter_map(|(slot, v)| cosine(query, v).map(|s| (*slot, s)))
            .collect();
        scored.sort_by(|a, b| {
            OrderedScore(b.1)
                .cmp(&OrderedScore(a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    /// Inserts `vectors` into `(model, scope)` one at a time, in slot order
    /// `0..vectors.len()`, both into `VECTORS`/`EMBEDDING_REF` (via
    /// `put_vector`) and into the HNSW graph (via `insert`, `id =
    /// NodeId::from_u128(slot + 1)` — `+1` so slot 0 never collides with a
    /// hypothetical id 0 edge case). Mirrors exactly what a real `apply_op`
    /// call sequence does, just without the op-log machinery.
    fn insert_incrementally(
        db: &Database,
        model: u32,
        scope: u32,
        vectors: &[Vec<f32>],
        params: &HnswParams,
    ) {
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            for (slot, v) in vectors.iter().enumerate() {
                put_vector(&mut vtab, &mut rtab, model, scope, slot as u64, v).unwrap();
                let reader = GraphReader {
                    vectors: &vtab,
                    refs: &rtab,
                    model,
                    scope,
                };
                insert(
                    &mut links,
                    &mut meta,
                    &reader,
                    params,
                    slot as u64,
                    NodeId::from_u128(slot as u128 + 1),
                    v,
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    fn search_cluster(
        db: &Database,
        model: u32,
        scope: u32,
        query: &[f32],
        ef: usize,
        k: usize,
    ) -> Vec<(u64, f32)> {
        let tx = db.begin_read().unwrap();
        let links = tx.open_table(HNSW_LINKS).unwrap();
        let meta_tab = tx.open_table(HNSW_META).unwrap();
        let vtab = tx.open_table(VECTORS).unwrap();
        let rtab = tx.open_table(EMBEDDING_REF).unwrap();
        let meta_row = read_meta(&meta_tab, model, scope).unwrap().unwrap();
        let reader = GraphReader {
            vectors: &vtab,
            refs: &rtab,
            model,
            scope,
        };
        search(&links, &meta_row, &reader, query, ef, k).unwrap()
    }

    /// Every `HNSW_LINKS` row for `(model, scope)` as `(slot, level, row)`,
    /// with the cluster-prefix bytes stripped from the key — the
    /// "modulo cluster prefix" comparator `build_cluster_is_equivalent_
    /// to_incremental_inserts` uses to compare two different clusters' rows.
    fn collect_link_rows(
        links: &impl ReadableTable<&'static [u8], &'static [u8]>,
        model: u32,
        scope: u32,
    ) -> Vec<(u64, u8, LinkRow)> {
        let prefix = link_prefix(model, scope);
        let mut start = prefix.to_vec();
        start.extend_from_slice(&[0u8; 9]);
        let mut end = prefix.to_vec();
        end.extend_from_slice(&[0xFFu8; 9]);
        let mut out = Vec::new();
        for entry in links.range(start.as_slice()..=end.as_slice()).unwrap() {
            let (k, v) = entry.unwrap();
            let key = k.value();
            let slot = u64::from_be_bytes(key[8..16].try_into().unwrap());
            let level = key[16];
            let raw = unframe_value(v.value()).unwrap();
            let row: LinkRow = postcard::from_bytes(&raw).unwrap();
            out.push((slot, level, row));
        }
        out
    }

    #[test]
    fn insert_then_search_finds_exact_neighbors_when_ef_covers_all() {
        let dim = 8;
        let n = 64;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_0001);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let entries: Vec<(u64, Vec<f32>)> = vectors
            .iter()
            .enumerate()
            .map(|(slot, v)| (slot as u64, v.clone()))
            .collect();

        let (ef, k) = (64usize, 10usize);
        let queries = seed_vectors(5, dim, 0x5EED_0002);
        for q in &queries {
            let got = search_cluster(&db, model, scope, q, ef, k);
            // `search`'s contract is "up to max(ef, k)" results, not just
            // `k` — with ef=64 >= n that's the whole (non-tomb) cluster.
            let want = brute_force(&entries, &HashSet::new(), q, ef.max(k));
            assert_eq!(
                got, want,
                "ef >= n must make HNSW search exactly equal brute force"
            );
        }
    }

    #[test]
    fn search_excludes_tombstones_but_routes_through_them() {
        let dim = 8;
        let n = 32;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_0003);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let entries: Vec<(u64, Vec<f32>)> = vectors
            .iter()
            .enumerate()
            .map(|(slot, v)| (slot as u64, v.clone()))
            .collect();

        let query = seed_vectors(1, dim, 0x5EED_0004).remove(0);
        // Find (and tombstone) the known top-1 for this query.
        let top1 = brute_force(&entries, &HashSet::new(), &query, 1);
        let tombstoned_slot = top1[0].0;

        let tx = db.begin_write().unwrap();
        {
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            let newly = tombstone(&mut links, &mut meta, model, scope, tombstoned_slot).unwrap();
            assert!(
                newly,
                "the slot must have been present and not already a tomb"
            );
            // Tombstoning it again must report false (not newly tombstoned).
            let again = tombstone(&mut links, &mut meta, model, scope, tombstoned_slot).unwrap();
            assert!(!again);
        }
        tx.commit().unwrap();

        let mut tombstoned = HashSet::new();
        tombstoned.insert(tombstoned_slot);
        let (ef, k) = (32usize, 10usize);
        // `search`'s contract is "up to max(ef, k)" results.
        let want = brute_force(&entries, &tombstoned, &query, ef.max(k));
        let got = search_cluster(&db, model, scope, &query, ef, k);
        assert_eq!(
            got, want,
            "tombstoned slot must be excluded from results but the rest must \
             still be exactly the brute-force top-k (it still routed)"
        );
        assert!(got.iter().all(|&(slot, _)| slot != tombstoned_slot));
    }

    #[test]
    fn zero_norm_vectors_never_enter_the_graph() {
        let dim = 8;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(5, dim, 0x5EED_0005);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let zero_slot = vectors.len() as u64;
        let zero_vec = vec![0.0f32; dim];

        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();

            let before = read_meta(&meta, model, scope).unwrap().unwrap();

            // Zero vector IS resolvable by slot (as a real embedding could
            // be), but `insert` must still refuse to wire it into the graph.
            put_vector(&mut vtab, &mut rtab, model, scope, zero_slot, &zero_vec).unwrap();
            let reader = GraphReader {
                vectors: &vtab,
                refs: &rtab,
                model,
                scope,
            };
            insert(
                &mut links,
                &mut meta,
                &reader,
                &params,
                zero_slot,
                NodeId::from_u128(999),
                &zero_vec,
            )
            .unwrap();

            let after = read_meta(&meta, model, scope).unwrap().unwrap();
            assert_eq!(
                before, after,
                "zero-norm insert must be a total no-op on meta (graph_len unchanged)"
            );
            assert!(
                read_links(&links, model, scope, zero_slot, 0)
                    .unwrap()
                    .is_none(),
                "zero-norm insert must write no link row"
            );
        }
        tx.commit().unwrap();

        let query = seed_vectors(1, dim, 0x5EED_0006).remove(0);
        let got = search_cluster(&db, model, scope, &query, 10, 5);
        assert!(
            got.iter().all(|&(slot, _)| slot != zero_slot),
            "search must never return the zero-norm slot"
        );
    }

    #[test]
    fn build_cluster_is_equivalent_to_incremental_inserts() {
        let dim = 8;
        let n = 48;
        // Two different MODELS (not two scopes under the same model): a
        // node's `(model, scope)` ref is scope-immutable for its lifetime
        // (`put_vector`'s doc comment / debug_assert), so re-embedding the
        // SAME slot numbers under the same model but a different scope is a
        // real invariant violation, not just a test-fixture wrinkle. Two
        // models is the legitimate way to give graph A and graph B their
        // own independent `VECTORS` rows while sharing slot numbers 0..n
        // (required for the "same keys modulo cluster prefix" comparison
        // below — the cluster prefix already encodes `(model, scope)`, so
        // only `model` needs to differ, `scope` can and does stay put).
        let model_a = 1;
        let model_b = 2;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_0007);
        let ids: Vec<NodeId> = (0..n).map(|i| NodeId::from_u128(i as u128 + 1)).collect();
        let (_dir, db) = open_db();

        // Graph A: plain incremental inserts (uses its own internal id
        // scheme, but with the SAME per-slot ids as graph B below).
        {
            let tx = db.begin_write().unwrap();
            {
                let mut vtab = tx.open_table(VECTORS).unwrap();
                let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
                let mut links = tx.open_table(HNSW_LINKS).unwrap();
                let mut meta = tx.open_table(HNSW_META).unwrap();
                for (slot, v) in vectors.iter().enumerate() {
                    put_vector(&mut vtab, &mut rtab, model_a, scope, slot as u64, v).unwrap();
                    let reader = GraphReader {
                        vectors: &vtab,
                        refs: &rtab,
                        model: model_a,
                        scope,
                    };
                    insert(
                        &mut links,
                        &mut meta,
                        &reader,
                        &params,
                        slot as u64,
                        ids[slot],
                        v,
                    )
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }

        // Graph B: same vectors written to VECTORS under the second model,
        // plus a slot->NodeId (`NODE_IDS`) mapping using the identical ids
        // (so `level_for` computes identically), then rebuilt via
        // `build_cluster` in one shot.
        {
            let tx = db.begin_write().unwrap();
            {
                let mut vtab = tx.open_table(VECTORS).unwrap();
                let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
                let mut slot_meta = tx.open_table(SLOT_ALLOC_META).unwrap();
                let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
                let mut node_ids = tx.open_table(NODE_IDS).unwrap();
                for (slot, v) in vectors.iter().enumerate() {
                    put_vector(&mut vtab, &mut rtab, model_b, scope, slot as u64, v).unwrap();
                    let alloc_slot =
                        alloc_node_slot(&mut slot_meta, &mut node_slots, &mut node_ids, ids[slot])
                            .unwrap();
                    assert_eq!(
                        alloc_slot, slot as u64,
                        "this test's id scheme must allocate slots in the same order as VECTORS"
                    );
                }

                let mut links = tx.open_table(HNSW_LINKS).unwrap();
                let mut meta = tx.open_table(HNSW_META).unwrap();
                let reader = GraphReader {
                    vectors: &vtab,
                    refs: &rtab,
                    model: model_b,
                    scope,
                };
                build_cluster(
                    &mut links, &mut meta, &vtab, &reader, &node_ids, &params, model_b, scope,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }

        let tx = db.begin_read().unwrap();
        let links = tx.open_table(HNSW_LINKS).unwrap();
        let meta_tab = tx.open_table(HNSW_META).unwrap();
        let rows_a = collect_link_rows(&links, model_a, scope);
        let rows_b = collect_link_rows(&links, model_b, scope);
        assert_eq!(
            rows_a, rows_b,
            "build_cluster must reproduce incremental-insert HNSW_LINKS rows \
             bit for bit, modulo the cluster prefix"
        );
        let meta_a = read_meta(&meta_tab, model_a, scope).unwrap().unwrap();
        let meta_b = read_meta(&meta_tab, model_b, scope).unwrap().unwrap();
        assert_eq!(
            meta_a, meta_b,
            "build_cluster must reproduce identical meta"
        );
    }

    // -- Task 3 carry-overs: zero-norm query, entry-point tombstone,
    // reinsert_links ----------------------------------------------------

    #[test]
    fn zero_norm_query_yields_empty_result() {
        // Carried from Task 2's review: `search`'s doc comment now states
        // this is by-design, not an error path — pin it.
        let dim = 8;
        let n = 32;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_0008);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let zero_query = vec![0.0f32; dim];
        let got = search_cluster(&db, model, scope, &zero_query, 16, 5);
        assert!(
            got.is_empty(),
            "a zero-norm query must yield an empty result, not an error or a scored hit"
        );
    }

    #[test]
    fn tombstoning_the_entry_point_still_routes_correctly() {
        // Carried from Task 2's review: the entry point is the one slot
        // `search`'s greedy descend ALWAYS starts from — tombstoning it must
        // not break routing to the rest of the graph, even though the
        // now-tombstoned entry itself must never appear in results.
        let dim = 8;
        let n = 40;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_0009);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let entry_slot = {
            let tx = db.begin_read().unwrap();
            let meta_tab = tx.open_table(HNSW_META).unwrap();
            read_meta(&meta_tab, model, scope)
                .unwrap()
                .unwrap()
                .entry_slot
        };

        let entries: Vec<(u64, Vec<f32>)> = vectors
            .iter()
            .enumerate()
            .map(|(slot, v)| (slot as u64, v.clone()))
            .collect();

        let tx = db.begin_write().unwrap();
        {
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            let newly = tombstone(&mut links, &mut meta, model, scope, entry_slot).unwrap();
            assert!(newly, "the entry slot must have had a live level-0 row");
        }
        tx.commit().unwrap();

        let mut tombstoned = HashSet::new();
        tombstoned.insert(entry_slot);
        let query = seed_vectors(1, dim, 0x5EED_000A).remove(0);
        let (ef, k) = (40usize, 10usize);
        let want = brute_force(&entries, &tombstoned, &query, ef.max(k));
        let got = search_cluster(&db, model, scope, &query, ef, k);
        assert_eq!(
            got, want,
            "tombstoning the entry point must still leave the rest of the graph \
             reachable (search still starts its greedy descend FROM the tombstoned \
             entry, it just never RANKS it)"
        );
        assert!(got.iter().all(|&(slot, _)| slot != entry_slot));
    }

    #[test]
    fn reinsert_links_rewires_own_row_without_touching_neighbors() {
        let dim = 8;
        let n = 40;
        let model = 1;
        let scope = 1;
        let params = HnswParams::default();
        let vectors = seed_vectors(n, dim, 0x5EED_000B);
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        // Snapshot every OTHER slot's link rows before the rewire.
        let target_slot: u64 = 3;
        let before_meta = {
            let tx = db.begin_read().unwrap();
            let meta_tab = tx.open_table(HNSW_META).unwrap();
            read_meta(&meta_tab, model, scope).unwrap().unwrap()
        };
        let before_rows: Vec<(u64, u8, LinkRow)> = {
            let tx = db.begin_read().unwrap();
            let links = tx.open_table(HNSW_LINKS).unwrap();
            collect_link_rows(&links, model, scope)
                .into_iter()
                .filter(|(slot, _, _)| *slot != target_slot)
                .collect()
        };

        // Re-embed `target_slot` with a fresh vector under the SAME model.
        let new_vector = seed_vectors(1, dim, 0x5EED_000C).remove(0);
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vtab, &mut rtab, model, scope, target_slot, &new_vector).unwrap();
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            let reader = GraphReader {
                vectors: &vtab,
                refs: &rtab,
                model,
                scope,
            };
            reinsert_links(
                &mut links,
                &mut meta,
                &reader,
                &params,
                target_slot,
                &new_vector,
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let after_meta = {
            let tx = db.begin_read().unwrap();
            let meta_tab = tx.open_table(HNSW_META).unwrap();
            read_meta(&meta_tab, model, scope).unwrap().unwrap()
        };
        assert_eq!(
            after_meta.stale,
            before_meta.stale + 1,
            "reinsert_links must bump stale by exactly 1"
        );
        assert_eq!(
            after_meta.graph_len, before_meta.graph_len,
            "reinsert_links must never touch graph_len"
        );

        let after_rows: Vec<(u64, u8, LinkRow)> = {
            let tx = db.begin_read().unwrap();
            let links = tx.open_table(HNSW_LINKS).unwrap();
            collect_link_rows(&links, model, scope)
                .into_iter()
                .filter(|(slot, _, _)| *slot != target_slot)
                .collect()
        };
        assert_eq!(
            before_rows, after_rows,
            "reinsert_links must not touch any OTHER slot's link rows"
        );

        // The rewired slot's own row(s) must never reference itself.
        let tx = db.begin_read().unwrap();
        let links = tx.open_table(HNSW_LINKS).unwrap();
        let target_rows: Vec<(u64, u8, LinkRow)> = collect_link_rows(&links, model, scope)
            .into_iter()
            .filter(|(slot, _, _)| *slot == target_slot)
            .collect();
        assert!(!target_rows.is_empty());
        for (_, _, row) in &target_rows {
            assert!(!row.tomb, "a live re-embed's row must not be tombstoned");
            assert!(
                !row.neighbors.contains(&target_slot),
                "a slot must never list itself as its own neighbor"
            );
        }

        // The re-embedded vector must be exactly what search finds it with.
        let got = search_cluster(&db, model, scope, &new_vector, 40, 1);
        assert_eq!(got[0].0, target_slot);
    }

    #[test]
    fn reinsert_links_on_unbuilt_cluster_is_a_no_op() {
        let model = 9;
        let scope = 9;
        let params = HnswParams::default();
        let (_dir, db) = open_db();
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            let v = vec![1.0f32, 0.0, 0.0, 0.0];
            put_vector(&mut vtab, &mut rtab, model, scope, 0, &v).unwrap();
            let reader = GraphReader {
                vectors: &vtab,
                refs: &rtab,
                model,
                scope,
            };
            reinsert_links(&mut links, &mut meta, &reader, &params, 0, &v).unwrap();
            assert!(
                read_meta(&meta, model, scope).unwrap().is_none(),
                "an unbuilt cluster must stay unbuilt after reinsert_links"
            );
        }
        tx.commit().unwrap();
    }

    #[test]
    fn cluster_vector_count_matches_row_count() {
        let dim = 4;
        let model = 1;
        let scope = 1;
        let vectors = seed_vectors(5, dim, 0x5EED_000D);
        let (_dir, db) = open_db();
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            for (slot, v) in vectors.iter().enumerate() {
                put_vector(&mut vtab, &mut rtab, model, scope, slot as u64, v).unwrap();
            }
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vtab = tx.open_table(VECTORS).unwrap();
        assert_eq!(cluster_vector_count(&vtab, model, scope).unwrap(), 5);
        assert_eq!(cluster_vector_count(&vtab, model, 2).unwrap(), 0);
    }

    // -- Heuristic neighbor selection (revisit arc: 100k recall gate) -------

    /// Unit vector at `deg` degrees in the plane — the 2-d geometry the
    /// selection tests are built on: cosine similarity between two of these
    /// is exactly the cosine of their angular separation, so "clumped vs
    /// diverse" is controlled directly in degrees.
    fn unit2(deg: f32) -> Vec<f32> {
        let r = deg.to_radians();
        vec![r.cos(), r.sin()]
    }

    /// A `select_neighbors` resolver over a plain slot-indexed table.
    fn table_resolver(
        table: &[Vec<f32>],
    ) -> impl FnMut(u64) -> Result<Option<Rc<Vec<f32>>>, TopoError> + '_ {
        move |slot: u64| Ok(table.get(slot as usize).cloned().map(Rc::new))
    }

    /// Candidates scored against `query`, in `search_layer`'s output order
    /// (score desc, slot asc) — the exact shape `insert` hands to selection.
    fn scored_candidates(query: &[f32], table: &[Vec<f32>]) -> Vec<(OrderedScore, u64)> {
        let mut out: Vec<(OrderedScore, u64)> = table
            .iter()
            .enumerate()
            .map(|(slot, v)| (OrderedScore(cosine(query, v).unwrap()), slot as u64))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out
    }

    #[test]
    fn heuristic_selection_prefers_diverse_over_clumped() {
        // a (5°) and b (8°) clump together on one side of the query (0°);
        // c (-25°) sits alone on the other side. Closest-2 keeps {a, b};
        // the heuristic must prune b (closer to already-kept a than to the
        // query: cos3° > cos8°) and keep the diverse c (cos30° < cos25°).
        let table = vec![unit2(5.0), unit2(8.0), unit2(-25.0)];
        let query = unit2(0.0);
        let selected = select_neighbors(
            &scored_candidates(&query, &table),
            2,
            table_resolver(&table),
        )
        .unwrap();
        assert_eq!(
            selected,
            vec![0, 2],
            "keep closest + diverse, prune the clump"
        );
    }

    #[test]
    fn heuristic_backfills_pruned_in_candidate_order() {
        // Clump {a=5°, b=8°, d=10°} plus diverse c=-25°, budget 3. The
        // heuristic keeps {a, c} and prunes both clump members; keep-pruned
        // backfill must then take b (the FIRST pruned in candidate order),
        // not d, and the final row keeps (score desc, slot asc) order.
        let table = vec![unit2(5.0), unit2(8.0), unit2(10.0), unit2(-25.0)];
        let query = unit2(0.0);
        let selected = select_neighbors(
            &scored_candidates(&query, &table),
            3,
            table_resolver(&table),
        )
        .unwrap();
        assert_eq!(
            selected,
            vec![0, 1, 3],
            "a + backfilled b + diverse c; d stays pruned"
        );
    }

    #[test]
    fn heuristic_duplicate_candidates_keep_lowest_slot_first() {
        // Two identical vectors: the lower slot wins the tie in candidate
        // order and is kept; the duplicate is pruned (it is exactly as
        // close to the kept copy as anything can be) and only returns via
        // backfill when the budget allows.
        let table = vec![unit2(5.0), unit2(5.0)];
        let query = unit2(0.0);
        let selected = select_neighbors(
            &scored_candidates(&query, &table),
            2,
            table_resolver(&table),
        )
        .unwrap();
        assert_eq!(
            selected,
            vec![0, 1],
            "slot 0 kept on the tie, slot 1 backfilled"
        );
        let selected_one = select_neighbors(
            &scored_candidates(&query, &table),
            1,
            table_resolver(&table),
        )
        .unwrap();
        assert_eq!(
            selected_one,
            vec![0],
            "no backfill room: the duplicate stays pruned"
        );
    }

    #[test]
    fn insert_wires_diverse_neighbors_not_closest_clump() {
        // Site-level: the same geometry as the unit test, driven through the
        // real `insert` path. After inserting clump {a=5°, b=8°} and diverse
        // c=-25°, a 0° node with m0=2 must wire to {a, c}, not closest-2
        // {a, b}.
        let model = 1;
        let scope = 1;
        let params = HnswParams {
            m: 2,
            m0: 2,
            ef_construction: 8,
            ..HnswParams::default()
        };
        let vectors = vec![unit2(5.0), unit2(8.0), unit2(-25.0), unit2(0.0)];
        let (_dir, db) = open_db();
        insert_incrementally(&db, model, scope, &vectors, &params);

        let tx = db.begin_read().unwrap();
        let links = tx.open_table(HNSW_LINKS).unwrap();
        let row = read_links(&links, model, scope, 3, 0).unwrap().unwrap();
        assert_eq!(
            row.neighbors,
            vec![0, 2],
            "diverse {{a, c}}, not the {{a, b}} clump"
        );
    }

    #[test]
    fn default_params_version_is_bumped_for_heuristic_selection() {
        // Selection policy is write-side graph structure: switching to the
        // heuristic MUST bump the stamped params version so pre-existing
        // graphs (built with closest-M) drain + rebuild on open via
        // `ensure_hnsw_params` instead of silently mixing policies.
        assert_eq!(HnswParams::default().version, 2);
    }

    // -- Insert-throughput: per-graph-op decoded-vector cache ---------------

    #[test]
    fn vec_cache_matches_direct_reads_and_caches_misses() {
        // The cache must be a pure memo over `read_vector_by_slot` + the
        // cluster check: an in-cluster slot resolves to the identical
        // vector, an out-of-cluster or absent slot resolves to None, and
        // BOTH outcomes are served from memory on the second ask (pinned
        // by dropping the underlying rows between the two asks — a cache
        // that re-reads would see the mutation; the graph-op contract is
        // point-in-time stability within one op).
        let model = 1;
        let scope = 1;
        let (_dir, db) = open_db();
        let v0 = unit2(5.0);
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vtab, &mut rtab, model, scope, 0, &v0).unwrap();
            put_vector(&mut vtab, &mut rtab, 9, 9, 1, &unit2(8.0)).unwrap();

            let reader = GraphReader {
                vectors: &vtab,
                refs: &rtab,
                model,
                scope,
            };
            let mut cache = VecCache::new();
            assert_eq!(
                cache.get(&reader, 0).unwrap().as_deref(),
                Some(&v0),
                "in-cluster slot resolves to the stored vector"
            );
            assert_eq!(
                cache.get(&reader, 1).unwrap(),
                None,
                "cross-cluster slot is unresolvable in this reader's cluster"
            );
            assert_eq!(cache.get(&reader, 2).unwrap(), None, "absent slot is None");
        }
        tx.commit().unwrap();

        // Second round against tables where slot 0's rows are GONE: hits
        // must come from the cache, proving no re-read happens.
        let tx = db.begin_write().unwrap();
        {
            let mut vtab = tx.open_table(VECTORS).unwrap();
            let mut rtab = tx.open_table(EMBEDDING_REF).unwrap();
            let mut cache = VecCache::new();
            {
                let reader = GraphReader {
                    vectors: &vtab,
                    refs: &rtab,
                    model,
                    scope,
                };
                assert_eq!(cache.get(&reader, 0).unwrap().as_deref(), Some(&v0));
            }
            crate::vector_store::remove_vector(&mut vtab, &mut rtab, 0).unwrap();
            {
                let reader = GraphReader {
                    vectors: &vtab,
                    refs: &rtab,
                    model,
                    scope,
                };
                assert_eq!(
                    cache.get(&reader, 0).unwrap().as_deref(),
                    Some(&v0),
                    "cached hit survives row removal — served from memory"
                );
                let mut fresh = VecCache::new();
                assert_eq!(
                    fresh.get(&reader, 0).unwrap(),
                    None,
                    "a fresh cache sees the removal"
                );
            }
        }
        tx.commit().unwrap();
    }
}
