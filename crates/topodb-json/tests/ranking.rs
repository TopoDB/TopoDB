//! End-to-end kind-aware ranking through the CANONICAL half-life map.
//!
//! The engine's own kind-differentiation tests hand-copy the half-life
//! constants (they can't depend on this crate); this test closes the drift
//! gap by running a real search through `memory_kind_half_life()` itself —
//! if the lifecycle constants and the ranking map ever diverge, or the map
//! stops resolving the `kind` prop the write path actually stores, this
//! fails while the engine tests stay green.
use topodb::{Db, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet, SearchOptions};
use topodb_json::{default_spec, memory_kind_half_life};

const DAY_MS: i64 = 86_400_000;

fn backdated(content: &str, kind: Option<&str>, scope: ScopeId, ts: i64, n: u128) -> Op {
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    if let Some(k) = kind {
        props.insert("kind".into(), PropValue::Str(k.into()));
    }
    Op::CreateNode {
        id: NodeId::from_u128(((ts as u128) << 80) | n),
        scope: Scope::Id(scope),
        label: "Memory".into(),
        props,
    }
}

/// Equal text, equal age: the canonical map must order procedural (365d
/// half-life) above kind-less (semantic default, 120d) above episodic (14d).
#[test]
fn canonical_map_orders_kinds_in_a_real_search() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), default_spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let ts = now - 30 * DAY_MS;

    let episodic = backdated("granite quarry report", Some("episodic"), s, ts, 1);
    let plain = backdated("granite quarry report", None, s, ts, 2);
    let procedural = backdated("granite quarry report", Some("procedural"), s, ts, 3);
    let ids: Vec<NodeId> = [&episodic, &plain, &procedural]
        .iter()
        .map(|op| match op {
            Op::CreateNode { id, .. } => *id,
            _ => unreachable!(),
        })
        .collect();
    db.submit(vec![episodic, plain, procedural]).unwrap();

    let options = SearchOptions {
        recency_weight: 0.3,
        recency_half_life_by_prop: Some(memory_kind_half_life()),
        now_ms: Some(now),
        ..SearchOptions::default()
    };
    let order: Vec<NodeId> = db
        .search_text_with(&scopes, "granite quarry", 10, &options)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert_eq!(
        order,
        vec![ids[2], ids[1], ids[0]],
        "canonical map must rank procedural > kind-less(semantic) > episodic"
    );
}
