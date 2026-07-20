//! Naive reference model for the disk-resident-read migration.
//!
//! This intentionally uses no engine internals: it is the stable oracle for
//! public read semantics while v3 replaces the snapshot implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use topodb::workload::{batches, WorkloadSpec};
use topodb::*;

mod reference {
    use super::*;

    #[derive(Default)]
    pub struct RefModel {
        pub nodes: HashMap<NodeId, NodeRecord>,
        pub edges: HashMap<EdgeId, EdgeRecord>,
        /// Every node's CURRENT embedding, keyed by node id — mirrors
        /// `NodeRecord::embedding` (one live `(model, vector)` pair per
        /// node), maintained in lockstep by `apply`: `SetEmbedding` upserts
        /// (replacing any prior entry, cross-model or same-model alike —
        /// there is only ever one live embedding per node, exactly like the
        /// engine's slab, which tombstones the old row on a model switch),
        /// `RemoveNode` clears.
        pub embeddings: HashMap<NodeId, (String, Vec<f32>)>,
    }

    impl RefModel {
        pub fn apply(&mut self, op: &Op) {
            match op {
                Op::CreateNode {
                    id,
                    scope,
                    label,
                    props,
                } => {
                    self.nodes.insert(
                        *id,
                        NodeRecord {
                            id: *id,
                            scope: *scope,
                            label: label.clone(),
                            props: props.clone(),
                            embedding: None,
                        },
                    );
                }
                Op::SetNodeProps { id, props } => {
                    if let Some(node) = self.nodes.get_mut(id) {
                        for (key, value) in props {
                            match value {
                                Some(value) => {
                                    node.props.insert(key.clone(), value.clone());
                                }
                                None => {
                                    node.props.remove(key);
                                }
                            }
                        }
                    }
                }
                Op::SetEmbedding { id, model, vector } => {
                    if let Some(node) = self.nodes.get_mut(id) {
                        node.embedding = Some((model.clone(), vector.clone()));
                        self.embeddings.insert(*id, (model.clone(), vector.clone()));
                    }
                }
                Op::RemoveNode { id } => {
                    self.nodes.remove(id);
                    self.embeddings.remove(id);
                    self.edges
                        .retain(|_, edge| edge.from != *id && edge.to != *id);
                }
                Op::CreateEdge {
                    id,
                    scope,
                    ty,
                    from,
                    to,
                    props,
                    valid_from,
                } => {
                    self.edges.insert(
                        *id,
                        EdgeRecord {
                            id: *id,
                            scope: *scope,
                            ty: ty.clone(),
                            from: *from,
                            to: *to,
                            props: props.clone(),
                            valid_from: valid_from.expect("test ops are resolved"),
                            valid_to: None,
                        },
                    );
                }
                Op::CloseEdge { id, valid_to } => {
                    if let Some(edge) = self.edges.get_mut(id) {
                        edge.valid_to = *valid_to;
                    }
                }
            }
        }

        pub fn get(&self, id: NodeId, scopes: &ScopeSet) -> Option<NodeRecord> {
            self.nodes
                .get(&id)
                .filter(|node| scopes.contains(node.scope))
                .cloned()
        }

        pub fn find_by_prop(
            &self,
            scopes: &ScopeSet,
            label: &str,
            prop: &str,
            value: &PropValue,
        ) -> Vec<NodeRecord> {
            let mut hits: Vec<_> = self
                .nodes
                .values()
                .filter(|node| {
                    node.label == label
                        && scopes.contains(node.scope)
                        && node.props.get(prop) == Some(value)
                })
                .cloned()
                .collect();
            hits.sort_by_key(|node| node.id);
            hits
        }

        /// Every node under `label`, restricted to `scopes`, sorted by id
        /// ascending — the comparison-friendly order (the engine's pinned
        /// `nodes_by_label` order groups by scope first; sorting both sides
        /// by id turns that into a set-equality check without caring about
        /// the grouping, exactly like `find_by_prop` above).
        pub fn find_by_label(&self, scopes: &ScopeSet, label: &str) -> Vec<NodeRecord> {
            let mut hits: Vec<_> = self
                .nodes
                .values()
                .filter(|node| node.label == label && scopes.contains(node.scope))
                .cloned()
                .collect();
            hits.sort_by_key(|node| node.id);
            hits
        }

        /// Newest-`k` under `label`, restricted to `scopes`: `find_by_label`
        /// re-sorted descending by id (ULIDs sort by mint time, so
        /// descending id = newest first) and truncated to `k`. Mirrors
        /// `Db::nodes_by_label_newest`'s contract exactly, including order —
        /// this is compared WITHOUT a re-sort on the engine side.
        pub fn newest_by_label(&self, scopes: &ScopeSet, label: &str, k: usize) -> Vec<NodeRecord> {
            let mut hits = self.find_by_label(scopes, label);
            hits.sort_by_key(|node| std::cmp::Reverse(node.id));
            hits.truncate(k);
            hits
        }

        /// Cosine-rank every stored embedding under `model` that is (a)
        /// scoped into `scopes` (via the owning node's scope) and (b), if
        /// `candidates` is given, restricted to that id set. Mirrors
        /// `vector.rs`'s search path exactly: skip zero-norm vectors (either
        /// side, via `cosine` below returning `None`), skip a dim mismatch
        /// against `query` silently — no error, matching the real engine's
        /// per-slab dim guard (`if slab.dim != q.vector.len() { continue }`)
        /// — sort score desc then `NodeId` asc (matching `Slab::top_k`'s
        /// tie-break, which is also `Db::search_vector`'s final merge-sort
        /// tie-break), then truncate to `k`. An unknown `model` naturally
        /// yields an empty result — no embedding entry has that model —
        /// never an error, matching the real engine's shape for that case.
        pub fn search_vector(
            &self,
            model: &str,
            scopes: &ScopeSet,
            query: &[f32],
            k: usize,
            candidates: Option<&[NodeId]>,
        ) -> Vec<(NodeId, f32)> {
            let allow: Option<HashSet<NodeId>> = candidates.map(|c| c.iter().copied().collect());
            let mut hits: Vec<(NodeId, f32)> = Vec::new();
            for (id, (emb_model, vector)) in &self.embeddings {
                if emb_model != model {
                    continue;
                }
                if let Some(allow) = &allow {
                    if !allow.contains(id) {
                        continue;
                    }
                }
                let Some(node) = self.nodes.get(id) else {
                    continue;
                };
                if !scopes.contains(node.scope) {
                    continue;
                }
                // Per-embedding dim skip; equivalent to the engine's whole-slab
                // `slab.dim != q.vector.len()` skip only because
                // `prevalidate_dims` guarantees one dim per live (model, scope) slab.
                if vector.len() != query.len() {
                    continue;
                }
                if let Some(score) = cosine(vector, query) {
                    hits.push((*id, score));
                }
            }
            hits.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
            hits.truncate(k);
            hits
        }

        pub fn traverse(
            &self,
            start: NodeId,
            scopes: &ScopeSet,
            max_hops: u8,
            as_of: i64,
            direction: Direction,
        ) -> (BTreeSet<NodeId>, BTreeSet<EdgeId>) {
            let mut nodes = BTreeSet::new();
            let mut edges = BTreeSet::new();
            let mut frontier = VecDeque::new();
            if self
                .nodes
                .get(&start)
                .is_some_and(|node| scopes.contains(node.scope))
            {
                nodes.insert(start);
                frontier.push_back((start, 0));
            }
            while let Some((current, hop)) = frontier.pop_front() {
                if hop >= max_hops {
                    continue;
                }
                for edge in self.edges.values() {
                    let other = match direction {
                        Direction::Out if edge.from == current => Some(edge.to),
                        Direction::In if edge.to == current => Some(edge.from),
                        Direction::Both if edge.from == current => Some(edge.to),
                        Direction::Both if edge.to == current => Some(edge.from),
                        _ => None,
                    };
                    let Some(other) = other else {
                        continue;
                    };
                    if !scopes.contains(edge.scope)
                        || edge.valid_from > as_of
                        || edge.valid_to.is_some_and(|valid_to| as_of >= valid_to)
                    {
                        continue;
                    }
                    if !self
                        .nodes
                        .get(&other)
                        .is_some_and(|node| scopes.contains(node.scope))
                    {
                        continue;
                    }
                    edges.insert(edge.id);
                    if nodes.insert(other) {
                        frontier.push_back((other, hop + 1));
                    }
                }
            }
            (nodes, edges)
        }
    }

    /// Bit-for-bit the same formula as `vector::cosine` (single accumulation
    /// pass over `a.iter().zip(b)`, `None` when either side's squared-norm
    /// accumulator is exactly `0.0`) — kept identical rather than merely
    /// equivalent so this oracle can never diverge from the engine by a
    /// rounding ULP.
    fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
        let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
        for (x, y) in a.iter().zip(b) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 {
            return None;
        }
        Some(dot / (na.sqrt() * nb.sqrt()))
    }
}

#[derive(Clone)]
enum Probe {
    Get {
        id: NodeId,
        scopes: ScopeSet,
    },
    Find {
        scopes: ScopeSet,
        label: &'static str,
        prop: &'static str,
        value: PropValue,
    },
    Label {
        scopes: ScopeSet,
        label: &'static str,
    },
    LabelNewest {
        scopes: ScopeSet,
        label: &'static str,
        k: usize,
    },
    Traverse {
        id: NodeId,
        scopes: ScopeSet,
        hops: u8,
        as_of: i64,
        direction: Direction,
    },
    Vector {
        scopes: ScopeSet,
        model: &'static str,
        query: Vec<f32>,
        k: usize,
        candidates: Option<Vec<NodeId>>,
    },
}

fn index_spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}

fn apply(db: &Db, model: &mut reference::RefModel, ops: Vec<Op>) {
    for op in &ops {
        model.apply(op);
    }
    db.submit(ops).unwrap();
}

fn assert_equivalent(db: &Db, model: &reference::RefModel, probes: &[Probe]) {
    for probe in probes {
        match probe {
            Probe::Get { id, scopes } => {
                assert_eq!(db.node(scopes, *id), model.get(*id, scopes), "get {id:?}")
            }
            Probe::Find {
                scopes,
                label,
                prop,
                value,
            } => {
                let mut actual = db.nodes_by_prop(scopes, label, prop, value).unwrap();
                actual.sort_by_key(|node| node.id);
                assert_eq!(
                    actual,
                    model.find_by_prop(scopes, label, prop, value),
                    "find {label}.{prop}"
                );
            }
            Probe::Label { scopes, label } => {
                let mut actual = db.nodes_by_label(scopes, label);
                actual.sort_by_key(|node| node.id);
                assert_eq!(actual, model.find_by_label(scopes, label), "label {label}");
            }
            Probe::LabelNewest { scopes, label, k } => {
                let actual = db.nodes_by_label_newest(scopes, label, *k);
                assert_eq!(
                    actual,
                    model.newest_by_label(scopes, label, *k),
                    "label-newest {label} k={k}"
                );
            }
            Probe::Traverse {
                id,
                scopes,
                hops,
                as_of,
                direction,
            } => {
                let actual = db
                    .traverse(&TraversalQuery {
                        scopes: scopes.clone(),
                        seeds: vec![*id],
                        max_hops: *hops,
                        edge_types: None,
                        direction: *direction,
                        as_of: Some(*as_of),
                    })
                    .unwrap();
                let actual_nodes = actual
                    .nodes
                    .into_iter()
                    .map(|node| node.id)
                    .collect::<BTreeSet<_>>();
                let actual_edges = actual
                    .edges
                    .into_iter()
                    .map(|edge| edge.id)
                    .collect::<BTreeSet<_>>();
                let (expected_nodes, expected_edges) =
                    model.traverse(*id, scopes, *hops, *as_of, *direction);
                assert_eq!(actual_nodes, expected_nodes, "traverse nodes from {id:?}");
                assert_eq!(actual_edges, expected_edges, "traverse edges from {id:?}");
            }
            Probe::Vector {
                scopes,
                model: vector_model,
                query,
                k,
                candidates,
            } => {
                let actual = db
                    .search_vector(&VectorQuery {
                        scopes: scopes.clone(),
                        model: vector_model.to_string(),
                        vector: query.clone(),
                        k: *k,
                        candidates: candidates.clone(),
                    })
                    .unwrap();
                let actual: Vec<(NodeId, f32)> = actual
                    .into_iter()
                    .map(|(rec, score)| (rec.id, score))
                    .collect();
                let expected =
                    model.search_vector(vector_model, scopes, query, *k, candidates.as_deref());
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "vector search {vector_model} result count"
                );
                for (i, ((aid, ascore), (eid, escore))) in
                    actual.iter().zip(expected.iter()).enumerate()
                {
                    assert_eq!(aid, eid, "vector search {vector_model} rank {i} id");
                    assert!(
                        (ascore - escore).abs() < 1e-4,
                        "vector search {vector_model} rank {i} score: engine {ascore} vs model {escore}"
                    );
                }
            }
        }
    }
}

#[test]
fn reference_model_matches_v2_engine_on_generated_workloads() {
    let spec = WorkloadSpec {
        memories: 500,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("d.redb"), index_spec()).unwrap();
    let mut model = reference::RefModel::default();
    let mut edge_ids = Vec::new();
    for batch in batches(&spec) {
        edge_ids.extend(batch.iter().filter_map(|op| match op {
            Op::CreateEdge { id, .. } => Some(*id),
            _ => None,
        }));
        apply(&db, &mut model, batch);
    }
    let closed_at = 1_700_000_100_000;
    for id in edge_ids.into_iter().step_by(7) {
        apply(
            &db,
            &mut model,
            vec![Op::CloseEdge {
                id,
                valid_to: Some(closed_at),
            }],
        );
    }
    for i in (0..spec.memories).step_by(13) {
        apply(&db, &mut model, vec![Op::RemoveNode { id: memory_id(i) }]);
    }
    for i in (0..spec.memories).filter(|i| i % 5 == 0 && i % 13 != 0) {
        let mut props = BTreeMap::new();
        props.insert("reviewed".to_string(), Some(PropValue::Bool(true)));
        apply(
            &db,
            &mut model,
            vec![Op::SetNodeProps {
                id: memory_id(i),
                props,
            }],
        );
    }

    // Final-review Finding 1's differential-oracle regression (adjudicated
    // fix item 4): `CreateNode` ids are mint-once — a duplicate-id
    // `CreateNode` is now rejected at live submit rather than silently
    // upserted (see `validate.rs::prevalidate_create_node_ids`), and the old
    // silent-upsert path is what left `LABEL_INDEX` permanently stale.
    // Re-create a STILL-LIVE memory node's id (`1` isn't a multiple of 13,
    // so it survived the `RemoveNode` loop above) under a different
    // label/scope. `db.submit` must reject it, and the reference model must
    // NOT apply it — treating a duplicate-id create as rejected, matching
    // the new validation, is exactly what lets the oracle own this class of
    // bug permanently rather than needing a one-off regression test.
    let dup_target = memory_id(1);
    let dup_err = db
        .submit(vec![Op::CreateNode {
            id: dup_target,
            scope: Scope::Id(ScopeId::from_u128(1)),
            label: "Impersonator".into(),
            props: BTreeMap::new(),
        }])
        .unwrap_err();
    assert!(
        matches!(dup_err, TopoError::Rejected(_)),
        "duplicate-id CreateNode must be rejected by live submit, got {dup_err:?}"
    );
    // No `model.apply` call: the reference model's job here is to agree
    // that the op never happened, not to replicate it.

    let own = ScopeId::from_u128(1);
    let scope2 = ScopeId::from_u128(2);
    let scopes = [
        ScopeSet::of(&[own]),
        ScopeSet::of(&[scope2]),
        ScopeSet::of(&[own]).with_shared(),
    ];

    // --- Purpose-built small corpus for vector-search probes, distinct from
    // the workload's bulk "bench-768" embeddings so scores are exact and
    // easy to reason about, split precisely across `own` and `scope2`. ---
    let vprobe_ids: Vec<NodeId> = (0..6).map(vprobe_node_id).collect();
    let vprobe_scopes = [
        Scope::Id(own),
        Scope::Id(own),
        Scope::Id(own),
        Scope::Id(scope2),
        Scope::Id(scope2),
        Scope::Id(scope2),
    ];
    let vprobe_vectors: [[f32; 3]; 6] = [
        [1.0, 0.0, 0.0],  // n0: own    — exact match to the probe query
        [0.0, 1.0, 0.0],  // n1: own    — orthogonal, score 0.0
        [0.0, 0.0, 0.0],  // n2: own    — zero-norm, must be skipped everywhere
        [1.0, 0.0, 0.0],  // n3: scope2 — ties n0's score across the scope merge
        [0.5, 0.5, 0.0],  // n4: scope2 — partial match
        [-1.0, 0.0, 0.0], // n5: scope2 — opposite direction, negative score
    ];
    let mut create_ops = Vec::new();
    for (i, id) in vprobe_ids.iter().enumerate() {
        create_ops.push(Op::CreateNode {
            id: *id,
            scope: vprobe_scopes[i],
            label: "VProbe".into(),
            props: BTreeMap::new(),
        });
    }
    apply(&db, &mut model, create_ops);
    let mut embed_ops = Vec::new();
    for (i, id) in vprobe_ids.iter().enumerate() {
        embed_ops.push(Op::SetEmbedding {
            id: *id,
            model: "vprobe".into(),
            vector: vprobe_vectors[i].to_vec(),
        });
    }
    apply(&db, &mut model, embed_ops);
    let vprobe_query = vec![1.0f32, 0.0, 0.0];

    // Discriminating k=1 tie: two nodes score identically (cosine 1.0,
    // identical vectors), but SLOT order (creation order) and NodeId order
    // disagree for this pair — the higher-ULID node is submitted FIRST (so
    // it lands in the lower slot), the lower-ULID node SECOND (higher
    // slot). A correct tie-break (NodeId asc, per `Slab::top_k`) must return
    // the LOWER-ULID node even though it was created/slotted second; a
    // slot-order (or creation-order) tie-break would wrongly return the
    // higher-ULID node instead. This case is discriminating precisely
    // because the two orders disagree here — it pins the tie-break contract
    // (NodeId asc, not slot asc) for later tasks that touch slot layout.
    let tie_high = NodeId::from_u128(0x0600_0000_0000_0000_0000_0000_0000_0002);
    let tie_low = NodeId::from_u128(0x0600_0000_0000_0000_0000_0000_0000_0001);
    assert!(
        tie_high > tie_low,
        "tie fixture must have tie_high's ULID above tie_low's"
    );
    apply(
        &db,
        &mut model,
        vec![
            Op::CreateNode {
                id: tie_high,
                scope: Scope::Id(own),
                label: "VProbe".into(),
                props: BTreeMap::new(),
            },
            Op::SetEmbedding {
                id: tie_high,
                model: "tie-model".into(),
                vector: vec![1.0, 0.0],
            },
        ],
    ); // submitted (and thus slotted) first
    apply(
        &db,
        &mut model,
        vec![
            Op::CreateNode {
                id: tie_low,
                scope: Scope::Id(own),
                label: "VProbe".into(),
                props: BTreeMap::new(),
            },
            Op::SetEmbedding {
                id: tie_low,
                model: "tie-model".into(),
                vector: vec![1.0, 0.0],
            },
        ],
    ); // submitted (and thus slotted) second, despite the lower ULID

    let mut probes = Vec::new();
    for i in 0..50 {
        probes.push(Probe::Get {
            id: memory_id(i),
            scopes: scopes[i % scopes.len()].clone(),
        });
    }
    for i in 0..20 {
        probes.push(Probe::Get {
            id: memory_id(i * 13),
            scopes: ScopeSet::of(&[own]),
        });
    }
    for i in [0, 7, 199, 499, 500] {
        probes.push(Probe::Find {
            scopes: ScopeSet::of(&[own]),
            label: "Entity",
            prop: "name",
            value: PropValue::Str(format!("entity-{i}")),
        });
    }
    // Label reads (F9-11 Task 8): "Memory" has been through both a
    // create-then-RemoveNode pass (i % 13 == 0) and a SetNodeProps pass
    // (i % 5 == 0, i % 13 != 0), so these exercise LABEL_INDEX maintenance
    // across removal AND property mutation, not just plain creates. Every
    // scope shape the workload already has (own alone, scope2 alone, own +
    // shared) is probed for both the full scan and the newest-k read, plus
    // a label that was never created (empty on both sides) and k edge
    // cases (0, exactly-corpus-sized, larger-than-corpus).
    for scope in &scopes {
        probes.push(Probe::Label {
            scopes: scope.clone(),
            label: "Memory",
        });
        probes.push(Probe::Label {
            scopes: scope.clone(),
            label: "Entity",
        });
        probes.push(Probe::Label {
            scopes: scope.clone(),
            label: "NoSuchLabel",
        });
        for k in [0, 1, 5, spec.memories, spec.memories * 2] {
            probes.push(Probe::LabelNewest {
                scopes: scope.clone(),
                label: "Memory",
                k,
            });
        }
        probes.push(Probe::LabelNewest {
            scopes: scope.clone(),
            label: "NoSuchLabel",
            k: 5,
        });
    }
    let times = [1_699_999_999_999, 1_700_000_000_250, 1_700_000_200_000];
    for i in 1..=20 {
        if i % 13 == 0 {
            continue;
        }
        for hops in 1..=4 {
            for (scope_index, scope) in scopes.iter().enumerate() {
                probes.push(Probe::Traverse {
                    id: memory_id(i),
                    scopes: scope.clone(),
                    hops,
                    as_of: times[(i + hops as usize + scope_index) % times.len()],
                    direction: [Direction::Out, Direction::In, Direction::Both]
                        [(i + hops as usize) % 3],
                });
            }
        }
    }
    // full-scope search — also exercises the zero-norm skip (n2 never appears)
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own]),
        model: "vprobe",
        query: vprobe_query.clone(),
        k: 10,
        candidates: None,
    });
    // multi-scope set — merges own + scope2 into one ranked result
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own, scope2]),
        model: "vprobe",
        query: vprobe_query.clone(),
        k: 10,
        candidates: None,
    });
    // candidates restriction — scores only the listed ids, across scopes
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own, scope2]),
        model: "vprobe",
        query: vprobe_query.clone(),
        k: 10,
        candidates: Some(vec![vprobe_ids[1], vprobe_ids[4], vprobe_ids[5]]),
    });
    // unknown model — expect empty, not an error
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own, scope2]),
        model: "does-not-exist",
        query: vprobe_query.clone(),
        k: 5,
        candidates: None,
    });
    // dim mismatch — 4-dim query against the 3-dim "vprobe" corpus, under
    // the owning scope: expect empty on BOTH sides, not an error (engine
    // skips the whole slab via `slab.dim != q.vector.len()`; the model
    // skips per-embedding).
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own]),
        model: "vprobe",
        query: vec![1.0f32, 0.0, 0.0, 0.0],
        k: 10,
        candidates: None,
    });
    // k larger than corpus — over the workload's bulk "bench-768" embeddings
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own]),
        model: "bench-768",
        query: vec![0.01f32; spec.embed_dim],
        k: 1_000, // far more than the ~92 surviving bench-768 embeddings
        candidates: None,
    });
    // k = 1 tie: NodeId asc must win over slot (creation) order — see the
    // tie_high/tie_low fixture built above for why this pair discriminates.
    probes.push(Probe::Vector {
        scopes: ScopeSet::of(&[own]),
        model: "tie-model",
        query: vec![1.0, 0.0],
        k: 1,
        candidates: None,
    });

    assert_equivalent(&db, &model, &probes);

    // Literal winner of the k=1 tie, asserted on BOTH sides independently —
    // differential agreement alone can't rule out engine and model sharing
    // the same latent tie-break bug; only naming the expected winner can.
    let tie_scopes = ScopeSet::of(&[own]);
    let engine_tie = db
        .search_vector(&VectorQuery {
            scopes: tie_scopes.clone(),
            model: "tie-model".into(),
            vector: vec![1.0, 0.0],
            k: 1,
            candidates: None,
        })
        .unwrap();
    assert_eq!(
        engine_tie[0].0.id, tie_low,
        "engine k=1 tie must resolve NodeId asc (tie_low), not slot/creation order (tie_high)"
    );
    let model_tie = model.search_vector("tie-model", &tie_scopes, &[1.0, 0.0], 1, None);
    assert_eq!(
        model_tie[0].0, tie_low,
        "model k=1 tie must resolve NodeId asc (tie_low), not insertion order"
    );
}

fn memory_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0100_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}

fn vprobe_node_id(i: u128) -> NodeId {
    NodeId::from_u128(0x0400_0000_0000_0000_0000_0000_0000_0000 | i)
}
