use topodb::{Db, PropValue, Scope, TimeAxis};
use topodb_warehouse::event::*;
use topodb_warehouse::{
    derive, tier, Warehouse, WarehouseConfig, ARTIFACT_LABEL, CHUNK_LABEL, HAS_CHUNK_EDGE,
};

const DAY: i64 = 86_400_000;
fn art(id: &str, ts: i64, content: &str) -> Event {
    Event {
        id: id.into(),
        ts,
        host: String::new(),
        kind: Kind::Artifact,
        v: EVENT_VERSION,
        source: Some(Source {
            harness: "cc".into(),
            session: "s".into(),
            scope: "shared".into(),
            turn: None,
            tool: None,
            cwd: None,
            agent: None,
        }),
        artifact: Some(Artifact {
            kind: ArtifactType::FileRead,
            locator: id.into(),
            bytes: 0,
            hash: None,
            redacted: false,
            redactions: vec![],
            redact_v: None,
            content: Some(content.into()),
            blob: None,
        }),
        op: None,
        marker: None,
    }
}

#[test]
fn artifacts_and_segments_move_down_tiers_monotonically() {
    let t = tempfile::tempdir().unwrap();
    let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
    let cfg = WarehouseConfig {
        hot_days: 1,
        warm_days: 3,
        retention_days: 5,
        spool_min_age_ms: 0,
        ..Default::default()
    };
    let mut wh = Warehouse::open(&t.path().join("w"), cfg).unwrap();
    let now = 100 * DAY;
    let mk = |i: &str, age_days: i64| art(i, now - age_days * DAY, &format!("body of {i}\n"));
    let s: String = [
        mk("fresh", 0),
        mk("warmish", 2),
        mk("coldish", 4),
        mk("ancient", 6),
    ]
    .iter()
    .map(encode_line)
    .collect();
    std::fs::write(wh.layout.spool.join("a.jsonl"), s).unwrap();
    wh.drain(now).unwrap();
    // seal so the segment can be tiered; its last_ts is `now` (fresh) so it must NOT move
    topodb_warehouse::segment::seal_open(&wh.layout, &mut wh.manifest).unwrap();
    wh.save().unwrap();
    derive(&db, &wh, None, now, false).unwrap();
    let set = topodb_json::scope_to_scope_set(Scope::Shared);
    let rep = tier(&db, &mut wh, now).unwrap();
    assert_eq!(
        (rep.to_warm, rep.to_cold, rep.to_expired, rep.purged),
        (1, 1, 1, 0)
    );
    assert_eq!((rep.segments_archived, rep.segments_stripped), (0, 0));
    let by_loc = |l: &str| {
        db.nodes_by_label_unbumped(&set, ARTIFACT_LABEL)
            .into_iter()
            .find(|n| n.props.get("locator") == Some(&PropValue::Str(l.into())))
            .unwrap()
    };
    let tier_of = |l: &str| match by_loc(l).props.get("tier") {
        Some(PropValue::Str(s)) => s.clone(),
        _ => panic!(),
    };
    assert_eq!(
        (
            tier_of("fresh"),
            tier_of("warmish"),
            tier_of("coldish"),
            tier_of("ancient")
        ),
        ("hot".into(), "warm".into(), "cold".into(), "expired".into())
    );
    // warm: chunk kept, text stripped, preview kept
    let w = by_loc("warmish");
    let ch = db
        .edges_from(
            &set,
            w.id,
            None,
            Some(HAS_CHUNK_EDGE),
            true,
            TimeAxis::Valid,
        )
        .unwrap();
    assert_eq!(ch.len(), 1);
    let c = db.node(&set, ch[0].to).unwrap();
    assert!(!c.props.contains_key("text") && c.props.contains_key("preview"));
    // cold: chunks gone
    let c2 = by_loc("coldish");
    assert!(db
        .edges_from(
            &set,
            c2.id,
            None,
            Some(HAS_CHUNK_EDGE),
            true,
            TimeAxis::Valid
        )
        .unwrap()
        .is_empty());
    assert_eq!(db.nodes_by_label_unbumped(&set, CHUNK_LABEL).len(), 2); // fresh + warmish
                                                                        // idempotent
    let rep2 = tier(&db, &mut wh, now).unwrap();
    assert_eq!((rep2.to_warm, rep2.to_cold, rep2.to_expired), (0, 0, 0));
    // segment tiering: an old sealed segment archives, an ancient one is stripped
    let old_ts = now - 4 * DAY;
    std::fs::write(
        wh.layout.spool.join("b.jsonl"),
        encode_line(&art("old", old_ts, "old body\n")),
    )
    .unwrap();
    wh.drain(old_ts).unwrap();
    topodb_warehouse::segment::seal_open(&wh.layout, &mut wh.manifest).unwrap();
    wh.save().unwrap();
    let rep3 = tier(&db, &mut wh, now).unwrap();
    assert_eq!(rep3.segments_archived, 1);
    let e = wh.manifest.segments.iter().find(|e| e.archived).unwrap();
    assert!(topodb_warehouse::segment::segment_path(&wh.layout, e).starts_with(&wh.layout.archive));
    let rep4 = tier(&db, &mut wh, now + 3 * DAY).unwrap(); // now old segment is > retention (7 > 5)
    assert_eq!(rep4.segments_stripped, 1);
    let e = wh
        .manifest
        .segments
        .iter()
        .find(|e| e.original_blake3.is_some())
        .unwrap();
    let (evs, _) = topodb_warehouse::segment::read_segment(&wh.layout, e).unwrap();
    assert!(evs.iter().all(|ev| ev
        .artifact
        .as_ref()
        .is_none_or(|a| a.content.is_none() && a.blob.is_none() && a.hash.is_some())));
    assert!(topodb_warehouse::segment::verify(&wh.layout, &wh.manifest).is_empty());
}
