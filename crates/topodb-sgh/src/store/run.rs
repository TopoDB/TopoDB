use std::collections::HashMap;

use topodb::{
    Db, Direction, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet, TraversalQuery,
};

use super::supersede::link_superseding;
use super::{
    SghError, EDGE_ATTEMPT_OF, EDGE_DEPENDS_ON, EDGE_HAS_STATE, EDGE_MEMBER_OF, EDGE_PRODUCED,
    EDGE_REVISION_OF, LABEL_ATTEMPT, LABEL_NODE, LABEL_OUTPUT, LABEL_REVISION, LABEL_RUN,
    LABEL_STATE,
};
use crate::schema::validate::Validated;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Recovering,
    Blocked,
    Skipped,
}

impl NodeState {
    pub const ALL: [NodeState; 8] = [
        NodeState::Pending,
        NodeState::Ready,
        NodeState::Running,
        NodeState::Succeeded,
        NodeState::Failed,
        NodeState::Recovering,
        NodeState::Blocked,
        NodeState::Skipped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Pending => "PENDING",
            NodeState::Ready => "READY",
            NodeState::Running => "RUNNING",
            NodeState::Succeeded => "SUCCEEDED",
            NodeState::Failed => "FAILED",
            NodeState::Recovering => "RECOVERING",
            NodeState::Blocked => "BLOCKED",
            NodeState::Skipped => "SKIPPED",
        }
    }

    // Inherent by choice: `Option` return (not `Result<_, Err>`) is the right
    // shape for a closed vocabulary, and implementing `std::str::FromStr`
    // would force an error type nobody consumes. CI's clippy 1.97 flags the
    // name; the ambiguity is acceptable here.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        NodeState::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Terminal states end a node's participation in the run.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NodeState::Succeeded | NodeState::Blocked | NodeState::Skipped
        )
    }
}

/// Persists a run's DAG, per-node state history, outputs, and failed
/// attempts. Every run lives in its own `Scope::Id` — no data written by
/// `RunStore` is ever shared-scope, so reads use a bare `ScopeSet::of(&[sid])`
/// (see `link_superseding`'s doc comment on why `.with_shared()` would be
/// wrong here: it would let out-of-scope edges participate in reads that are
/// supposed to be scoped to this one run).
pub struct RunStore {
    db: Db,
    scope: Scope,
    scopes: ScopeSet,
    run_node: NodeId,
    /// graph node id -> engine node id
    nodes: HashMap<String, NodeId>,
    /// state name -> engine node id
    states: HashMap<&'static str, NodeId>,
}

impl RunStore {
    pub fn create(db: &Db, run_id: &str, v: &Validated, now_ms: i64) -> Result<Self, SghError> {
        let sid = ScopeId::new();
        let scope = Scope::Id(sid);
        let scopes = ScopeSet::of(&[sid]);

        let run_node = NodeId::new();
        let mut props = Props::new();
        props.insert("run_id".into(), PropValue::Str(run_id.to_string()));
        props.insert("goal".into(), PropValue::Str(v.graph.goal.clone()));
        let mut ops = vec![Op::CreateNode {
            id: run_node,
            scope,
            label: LABEL_RUN.into(),
            props,
        }];

        // One state node per variant, per run.
        let mut states = HashMap::new();
        for st in NodeState::ALL {
            let id = NodeId::new();
            let mut p = Props::new();
            p.insert("name".into(), PropValue::Str(st.as_str().to_string()));
            ops.push(Op::CreateNode {
                id,
                scope,
                label: LABEL_STATE.into(),
                props: p,
            });
            states.insert(st.as_str(), id);
        }

        // One graph node per declared node.
        let mut nodes = HashMap::new();
        for n in &v.graph.nodes {
            let id = NodeId::new();
            let mut p = Props::new();
            p.insert("node_id".into(), PropValue::Str(n.id.clone()));
            p.insert(
                "kind".into(),
                PropValue::Str(format!("{:?}", n.kind).to_lowercase()),
            );
            p.insert("retries".into(), PropValue::Int(n.budget.retries as i64));
            p.insert("repairs".into(), PropValue::Int(n.budget.repairs as i64));
            ops.push(Op::CreateNode {
                id,
                scope,
                label: LABEL_NODE.into(),
                props: p,
            });
            nodes.insert(n.id.clone(), id);
        }

        db.submit_at(ops, now_ms)?;

        let store = RunStore {
            db: db.clone(),
            scope,
            scopes,
            run_node,
            nodes,
            states,
        };

        // Structural edges, then initial state. These are plain creates, not
        // supersessions — DEPENDS_ON is many-valued and must not self-close.
        let mut ops = Vec::new();
        for n in &v.graph.nodes {
            let from = store.nodes[&n.id];
            ops.push(Op::CreateEdge {
                id: EdgeId::new(),
                scope,
                ty: EDGE_MEMBER_OF.into(),
                from,
                to: run_node,
                props: Props::new(),
                valid_from: Some(now_ms),
            });
            for need in &n.needs {
                ops.push(Op::CreateEdge {
                    id: EdgeId::new(),
                    scope,
                    ty: EDGE_DEPENDS_ON.into(),
                    from,
                    to: store.nodes[need],
                    props: Props::new(),
                    valid_from: Some(now_ms),
                });
            }
        }
        db.submit_at(ops, now_ms)?;

        for n in &v.graph.nodes {
            store.set_state(&n.id, NodeState::Pending, now_ms)?;
        }

        Ok(store)
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn run_node(&self) -> NodeId {
        self.run_node
    }

    pub fn set_state(&self, node_id: &str, state: NodeState, now_ms: i64) -> Result<(), SghError> {
        let from = self.nodes[node_id];
        let to = self.states[state.as_str()];
        link_superseding(&self.db, self.scope, from, to, EDGE_HAS_STATE, now_ms)?;
        Ok(())
    }

    /// Current state, i.e. the still-open `HAS_STATE` edge. Reads with a
    /// sentinel `as_of` far in the future rather than `as_of: None` (wall
    /// clock): every write in this crate goes through `submit_at` with an
    /// explicit, caller-supplied `now_ms` — often small/synthetic in tests —
    /// so anchoring "latest" to the real wall clock would work today (the
    /// real clock is always later) but ties correctness to an environmental
    /// fact that has nothing to do with the run. The sentinel makes "read
    /// whatever is open" deterministic and independent of when the test (or
    /// process) happens to run.
    pub fn state(&self, node_id: &str) -> Result<NodeState, SghError> {
        let from = self.nodes[node_id];
        // No `edges_from` exists on the engine — `traverse` is the public read
        // that answers "open edges of this type out of this node". See Task 4.
        let sg = self.db.traverse(&TraversalQuery {
            scopes: self.scopes.clone(),
            seeds: vec![from],
            max_hops: 1,
            edge_types: Some(vec![EDGE_HAS_STATE.into()]),
            direction: Direction::Out,
            as_of: Some(i64::MAX - 1),
        })?;
        let open: Vec<_> = sg.edges.iter().filter(|e| e.from == from).collect();
        let edge = open
            .first()
            .expect("every node has exactly one open state edge");
        let rec = self
            .db
            .node(&self.scopes, edge.to)
            .expect("state node exists");
        let name = match rec.props.get("name") {
            Some(PropValue::Str(s)) => s.clone(),
            _ => unreachable!("state node always carries a name"),
        };
        Ok(NodeState::from_str(&name).expect("known state name"))
    }

    /// Historical state read. Uses `traverse`, the only read that honours
    /// `as_of` (`read.rs`); one hop suffices for a node's state edge.
    pub fn state_at(&self, node_id: &str, as_of: i64) -> Result<Option<NodeState>, SghError> {
        let from = self.nodes[node_id];
        let q = TraversalQuery {
            scopes: self.scopes.clone(),
            seeds: vec![from],
            max_hops: 1,
            edge_types: Some(vec![EDGE_HAS_STATE.into()]),
            direction: Direction::Out,
            as_of: Some(as_of),
        };
        let sub = self.db.traverse(&q)?;
        for rec in sub.nodes.iter() {
            if rec.label == LABEL_STATE {
                if let Some(PropValue::Str(name)) = rec.props.get("name") {
                    return Ok(NodeState::from_str(name));
                }
            }
        }
        Ok(None)
    }

    /// Outputs are a mutable current value, same as node state: a fresh
    /// `SghOutput` node is created on every call, then linked with
    /// `link_superseding` as `node -[EDGE_PRODUCED]-> output` so the prior
    /// output edge for this node (if any) is closed rather than left open
    /// alongside the new one. The edge direction matters here — supersession
    /// is keyed on `(from, ty)`, so it must key on the stable node id, not on
    /// the freshly-created output id (which would never match a prior edge).
    pub fn record_output(&self, node_id: &str, json: &str, now_ms: i64) -> Result<(), SghError> {
        let node = self.nodes[node_id];
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(json.to_string()));
        self.db.submit_at(
            vec![Op::CreateNode {
                id,
                scope: self.scope,
                label: LABEL_OUTPUT.into(),
                props,
            }],
            now_ms,
        )?;
        link_superseding(&self.db, self.scope, node, id, EDGE_PRODUCED, now_ms)?;
        Ok(())
    }

    /// Current output, i.e. the still-open `PRODUCED` edge's target. Mirrors
    /// `state()`: reads with a sentinel `as_of` far in the future for the
    /// same reason (see `state()`'s doc comment) rather than `as_of: None`.
    pub fn output(&self, node_id: &str) -> Result<Option<String>, SghError> {
        let node = self.nodes[node_id];
        let q = TraversalQuery {
            scopes: self.scopes.clone(),
            seeds: vec![node],
            max_hops: 1,
            edge_types: Some(vec![EDGE_PRODUCED.into()]),
            direction: Direction::Out,
            as_of: Some(i64::MAX - 1),
        };
        let sub = self.db.traverse(&q)?;
        let open: Vec<_> = sub.edges.iter().filter(|e| e.from == node).collect();
        let Some(edge) = open.first() else {
            return Ok(None);
        };
        let rec = self
            .db
            .node(&self.scopes, edge.to)
            .expect("output node exists");
        match rec.props.get("content") {
            Some(PropValue::Str(c)) => Ok(Some(c.clone())),
            _ => Ok(None),
        }
    }

    pub fn record_attempt(
        &self,
        node_id: &str,
        rung: &str,
        error: &str,
        now_ms: i64,
    ) -> Result<(), SghError> {
        let node = self.nodes[node_id];
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("rung".into(), PropValue::Str(rung.to_string()));
        props.insert("error".into(), PropValue::Str(error.to_string()));
        props.insert("at".into(), PropValue::DateTime(now_ms));
        self.db.submit_at(
            vec![
                Op::CreateNode {
                    id,
                    scope: self.scope,
                    label: LABEL_ATTEMPT.into(),
                    props,
                },
                Op::CreateEdge {
                    id: EdgeId::new(),
                    scope: self.scope,
                    ty: EDGE_ATTEMPT_OF.into(),
                    from: id,
                    to: node,
                    props: Props::new(),
                    valid_from: Some(now_ms),
                },
            ],
            now_ms,
        )?;
        Ok(())
    }

    /// Mirrors `state()` and `output()`: reads with a sentinel `as_of` far in
    /// the future rather than `as_of: None` (wall clock). Every write here
    /// goes through `submit_at` with an explicit, caller-supplied `now_ms`,
    /// so anchoring "current" to the real wall clock only happens to work
    /// when a caller's timestamps stay behind it — a caller stamping runs
    /// with future-dated timestamps (nothing in this crate forbids that)
    /// would see `as_of: None` treat every attempt as not-yet-valid and get
    /// an empty history back with no error. The sentinel makes the read
    /// deterministic and independent of wall time.
    pub fn attempts(&self, node_id: &str) -> Result<Vec<(String, String)>, SghError> {
        let node = self.nodes[node_id];
        let q = TraversalQuery {
            scopes: self.scopes.clone(),
            seeds: vec![node],
            max_hops: 1,
            edge_types: Some(vec![EDGE_ATTEMPT_OF.into()]),
            direction: Direction::In,
            as_of: Some(i64::MAX - 1),
        };
        let sub = self.db.traverse(&q)?;
        let mut out = Vec::new();
        for rec in sub.nodes.iter() {
            if rec.label == LABEL_ATTEMPT {
                let rung = match rec.props.get("rung") {
                    Some(PropValue::Str(s)) => s.clone(),
                    _ => continue,
                };
                let err = match rec.props.get("error") {
                    Some(PropValue::Str(s)) => s.clone(),
                    _ => String::new(),
                };
                out.push((rung, err));
            }
        }
        Ok(out)
    }

    /// Record a proposed successor graph for this run.
    ///
    /// Superseding: an earlier proposal is closed rather than deleted, so the
    /// chain of what was proposed and when stays recoverable. The graph is
    /// stored as YAML text because engine props are scalars only.
    pub fn record_revision(&self, yaml: &str, reason: &str, now_ms: i64) -> Result<(), SghError> {
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("yaml".into(), PropValue::Str(yaml.to_string()));
        props.insert("reason".into(), PropValue::Str(reason.to_string()));
        props.insert("at".into(), PropValue::DateTime(now_ms));

        self.db.submit_at(
            vec![Op::CreateNode {
                id,
                scope: self.scope,
                label: LABEL_REVISION.into(),
                props,
            }],
            now_ms,
        )?;

        link_superseding(
            &self.db,
            self.scope,
            self.run_node,
            id,
            EDGE_REVISION_OF,
            now_ms,
        )?;
        Ok(())
    }

    /// The currently-open proposed successor graph, if any, as `(yaml, reason)`.
    ///
    /// Uses `Db::edges_from(open_only: true)` — the engine's designated
    /// supersession primitive — rather than the traverse-plus-sentinel
    /// workaround the older reads in this file use. That method did not exist
    /// when they were written; new code should prefer it.
    pub fn revision(&self) -> Result<Option<(String, String)>, SghError> {
        let open = self.db.edges_from(
            &self.scopes,
            self.run_node,
            None,
            Some(EDGE_REVISION_OF),
            true,
        )?;

        let Some(edge) = open.first() else {
            return Ok(None);
        };
        let Some(rec) = self.db.node(&self.scopes, edge.to) else {
            return Ok(None);
        };

        let yaml = match rec.props.get("yaml") {
            Some(PropValue::Str(s)) => s.clone(),
            _ => return Ok(None),
        };
        let reason = match rec.props.get("reason") {
            Some(PropValue::Str(s)) => s.clone(),
            _ => String::new(),
        };
        Ok(Some((yaml, reason)))
    }
}
