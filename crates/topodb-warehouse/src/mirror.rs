//! Mirror the engine's op log into op events (spec §6.2 step 2). Pull-based
//! (`ops_since`), watermarked in META — gap-free by construction.
use crate::event::{new_ulid, Event, Kind, OpEvent, EVENT_VERSION};
use crate::manifest::{Manifest, MirrorGap};
use crate::{Layout, WarehouseError};
use serde::Serialize;
use topodb::{Db, TopoError};

pub const MIRRORED_SEQ_KEY: &str = "warehouse:mirrored_seq";

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MirrorReport {
    pub from_seq: u64,
    pub to_seq: u64,
    pub events: usize,
    pub gap: Option<MirrorGap>,
}

pub fn mirrored_seq(db: &Db) -> Result<u64, TopoError> {
    Ok(db
        .get_meta(MIRRORED_SEQ_KEY)?
        .and_then(|b| {
            std::str::from_utf8(&b)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(0))
}

pub fn mirror(
    db: &Db,
    layout: &Layout,
    m: &mut Manifest,
    now_ms: i64,
) -> Result<MirrorReport, WarehouseError> {
    let since = mirrored_seq(db)? + 1;
    let mut rep = MirrorReport {
        from_seq: since,
        ..Default::default()
    };
    let changes = match db.ops_since(since) {
        Ok(c) => c,
        Err(TopoError::Compacted { oldest }) => {
            let gap = MirrorGap {
                from: since,
                to: oldest.saturating_sub(1),
            };
            m.gaps.push(gap.clone());
            rep.gap = Some(gap);
            rep.from_seq = oldest;
            db.ops_since(oldest)?
        }
        Err(e) => return Err(e.into()),
    };
    if changes.is_empty() {
        rep.to_seq = rep.from_seq.saturating_sub(1);
        if let Some(gap) = &rep.gap {
            // Nothing survives above the compaction floor: advance the watermark
            // past the gap so idle ticks stop re-recording it.
            m.save(layout)?;
            db.set_meta(MIRRORED_SEQ_KEY, gap.to.to_string().as_bytes())?;
        } else if db.get_meta(MIRRORED_SEQ_KEY)?.is_none() {
            // Initialize the watermark when it does not exist yet (empty op log case).
            db.set_meta(MIRRORED_SEQ_KEY, rep.to_seq.to_string().as_bytes())?;
        }
        return Ok(rep);
    }
    let events: Vec<Event> = changes
        .iter()
        .map(|c| Event {
            id: new_ulid(),
            ts: now_ms,
            host: m.host_id.clone(),
            kind: Kind::Op,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            marker: None,
            op: Some(OpEvent {
                seq: c.seq,
                body: serde_json::to_value(&*c.op).expect("op serializes"),
            }),
        })
        .collect();
    let last = changes.last().expect("non-empty").seq;
    crate::segment::append_events(layout, m, &events, now_ms)?;
    m.save(layout)?;
    db.set_meta(MIRRORED_SEQ_KEY, last.to_string().as_bytes())?;
    rep.to_seq = last;
    rep.events = events.len();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Warehouse, WarehouseConfig};
    use topodb::{Db, NodeId, Op, Scope};

    fn create(db: &Db, n: usize) -> Vec<NodeId> {
        (0..n)
            .map(|i| {
                let id = NodeId::new();
                let mut props = topodb::Props::new();
                props.insert("content".into(), topodb::PropValue::Str(format!("m{i}")));
                db.submit(vec![Op::CreateNode {
                    id,
                    scope: Scope::Shared,
                    label: "Memory".into(),
                    props,
                }])
                .unwrap();
                id
            })
            .collect()
    }

    #[test]
    fn mirror_tails_from_watermark_and_records_seq() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), WarehouseConfig::default()).unwrap();
        create(&db, 3);
        let r = wh.mirror(&db, 100).unwrap();
        assert_eq!((r.from_seq, r.to_seq, r.events), (1, 3, 3));
        assert_eq!(mirrored_seq(&db).unwrap(), 3);
        let r2 = wh.mirror(&db, 101).unwrap();
        assert_eq!(r2.events, 0);
        create(&db, 2);
        let r3 = wh.mirror(&db, 102).unwrap();
        assert_eq!((r3.from_seq, r3.to_seq, r3.events), (4, 5, 2));
        let evs = wh.events().unwrap();
        let seqs: Vec<u64> = evs
            .iter()
            .filter_map(|e| e.op.as_ref().map(|o| o.seq))
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        // body round-trips to a real Op
        let op: Op = serde_json::from_value(evs[0].op.as_ref().unwrap().body.clone()).unwrap();
        assert!(matches!(op, Op::CreateNode { .. }));
        assert_eq!(wh.manifest.open_entry().unwrap().op_seq_max, Some(5));
    }

    #[test]
    fn compaction_below_watermark_is_recorded_as_gap() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), WarehouseConfig::default()).unwrap();
        create(&db, 5);
        db.compact_ops(4).unwrap(); // ops 1..3 gone before we ever mirrored
        let r = wh.mirror(&db, 100).unwrap();
        assert_eq!(r.gap, Some(crate::manifest::MirrorGap { from: 1, to: 3 }));
        assert_eq!((r.from_seq, r.to_seq, r.events), (4, 5, 2));
        assert_eq!(wh.manifest.gaps.len(), 1);
    }

    #[test]
    fn gap_with_empty_tail_advances_watermark_once() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), WarehouseConfig::default()).unwrap();
        create(&db, 3);
        db.compact_ops(4).unwrap(); // keep_from == current_seq + 1 legally empties the log
        let r = wh.mirror(&db, 100).unwrap();
        assert_eq!(r.gap, Some(crate::manifest::MirrorGap { from: 1, to: 3 }));
        assert_eq!((r.from_seq, r.to_seq, r.events), (4, 3, 0));
        assert_eq!(mirrored_seq(&db).unwrap(), 3);
        let r2 = wh.mirror(&db, 101).unwrap();
        assert_eq!((r2.gap, r2.events), (None, 0));
        assert_eq!(wh.manifest.gaps.len(), 1);
        // and new ops after the gap mirror normally
        create(&db, 1);
        let r3 = wh.mirror(&db, 102).unwrap();
        assert_eq!((r3.from_seq, r3.to_seq, r3.events), (4, 4, 1));
    }

    #[test]
    fn mirror_on_empty_log_initializes_watermark_to_zero() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), WarehouseConfig::default()).unwrap();
        assert!(db.get_meta(MIRRORED_SEQ_KEY).unwrap().is_none());
        let r = wh.mirror(&db, 100).unwrap();
        assert_eq!((r.from_seq, r.to_seq, r.events, r.gap), (1, 0, 0, None));
        assert_eq!(
            db.get_meta(MIRRORED_SEQ_KEY).unwrap().as_deref(),
            Some(b"0".as_slice())
        );
        assert_eq!(mirrored_seq(&db).unwrap(), 0);
    }
}
