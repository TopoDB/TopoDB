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
use std::collections::{BinaryHeap, HashSet};

#[allow(dead_code)]
pub(crate) const HNSW_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_meta");
#[allow(dead_code)]
pub(crate) const HNSW_LINKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_links");
pub(crate) const HNSW_META_FORMAT_V0: u8 = 0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HnswParams {
    pub version: u32,
    pub m: u32,
    pub m0: u32,
    pub ef_construction: u32,
    pub level_cap: u8,
    pub build_threshold: u64,
    pub rebuild_num: u32,
    pub rebuild_den: u32,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            version: 1,
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
    #[allow(dead_code)]
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
#[allow(dead_code)] // wired up by Task 3's applier
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
                slot: u64|
     -> Result<(), TopoError> {
        if !visited.insert(slot) {
            return Ok(());
        }
        let Some((m, s, v)) = read_vector_by_slot(reader.vectors, reader.refs, slot)? else {
            return Ok(());
        };
        if m != reader.model || s != reader.scope {
            return Ok(());
        }
        let Some(score) = cosine(query, &v) else {
            return Ok(());
        };
        let os = OrderedScore(score);
        candidates.push((os, Reverse(slot)));
        let is_tomb = read_links(links, reader.model, reader.scope, slot, level)?
            .map(|row| row.tomb)
            .unwrap_or(false);
        if !is_tomb {
            results.push(Reverse((os, Reverse(slot))));
            if results.len() > ef.max(1) {
                results.pop();
            }
        }
        Ok(())
    };

    for &slot in entry_pts {
        seed(&mut visited, &mut candidates, &mut results, slot)?;
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
            seed(&mut visited, &mut candidates, &mut results, nbr_slot)?;
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

/// Re-ranks `neighbor_slot`'s link row at `level` after appending
/// `new_slot`, by cosine distance FROM THE NEIGHBOR (not from the original
/// query) to every one of its (possibly now `max_m + 1`) neighbors, closest
/// first, slot-ascending on ties — then truncates to `max_m`. No-op if the
/// row is already within budget after the append.
fn prune_neighbor<V, R>(
    links: &mut Table<'_, &'static [u8], &'static [u8]>,
    reader: &GraphReader<'_, V, R>,
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
        match read_vector_by_slot(reader.vectors, reader.refs, neighbor_slot)? {
            Some((_, _, nv)) => {
                let mut scored: Vec<(OrderedScore, u64)> = Vec::with_capacity(row.neighbors.len());
                for &cand in &row.neighbors {
                    if let Some((_, _, cv)) =
                        read_vector_by_slot(reader.vectors, reader.refs, cand)?
                    {
                        if let Some(score) = cosine(&nv, &cv) {
                            scored.push((OrderedScore(score), cand));
                        }
                    }
                }
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                scored.truncate(max_m);
                row.neighbors = scored.into_iter().map(|(_, s)| s).collect();
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
/// down to 0, wiring itself to the closest `M`/`M0` (`M0` at level 0) and
/// pruning each of those neighbors back down to its own budget.
#[allow(dead_code)] // wired up by Task 3's applier
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
        let hits = search_layer(links, reader, &[entry_slot], vector, 1, descend_level)?;
        if let Some(&(_, best)) = hits.first() {
            entry_slot = best;
        }
        if descend_level == 0 {
            break;
        }
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
        let candidates = search_layer(links, reader, &entry_pts, vector, ef_c, cur_level)?;
        let max_m = if cur_level == 0 { params.m0 } else { params.m } as usize;
        let selected: Vec<u64> = candidates.iter().take(max_m).map(|&(_, s)| s).collect();

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
            prune_neighbor(links, reader, nbr, slot, cur_level, max_m)?;
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

/// Marks `slot`'s level-0 link row as tombstoned — `Ok(false)` if the slot
/// has no level-0 row (never inserted, or already removed from the graph
/// some other way) or is already tombstoned, `Ok(true)` if this call is the
/// one that newly tombstoned it. Only flips the flag and bumps `meta.stale`;
/// the row's `neighbors` (routing structure) are left exactly as they were,
/// per the module's "tombs route but don't rank" contract.
#[allow(dead_code)] // wired up by Task 3's applier
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

/// Greedy-descends from `meta_row`'s entry point (ef=1 per level down to 1),
/// then runs one `search_layer` at level 0 with `ef = max(ef, k)`, returning
/// up to that many non-tombstoned `(slot, exact cosine)` pairs sorted
/// `(score desc, slot asc)`. Callers resolve `NodeId`s and apply any further
/// `(score desc, NodeId asc)` re-sort themselves (`vector.rs`, as today).
#[allow(dead_code)] // wired up by Task 4's read path
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
    let mut entry_slot = meta_row.entry_slot;
    let mut cur_level = meta_row.entry_level;
    while cur_level > 0 {
        let hits = search_layer(links, reader, &[entry_slot], query, 1, cur_level)?;
        if let Some(&(_, best)) = hits.first() {
            entry_slot = best;
        }
        cur_level -= 1;
    }
    let hits = search_layer(links, reader, &[entry_slot], query, ef_eff, 0)?;
    Ok(hits
        .into_iter()
        .map(|(score, slot)| (slot, score.0))
        .collect())
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
#[allow(dead_code)] // wired up by Task 3's rebuild trigger
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
    let mut rows: Vec<(u64, Vec<f32>)> = Vec::new();
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
        rows.push((slot, vector));
    }

    for (slot, vector) in rows {
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
}
