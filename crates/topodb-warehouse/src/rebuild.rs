//! Rebuild a redb from op events alone (spec §7 `rebuild`): same segments ⇒ same DB.
use crate::event::Kind;
use crate::manifest::MirrorGap;
use crate::{Warehouse, WarehouseError};
use serde::Serialize;
use std::path::Path;
use topodb::{Db, IndexSpec, Op};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RebuildReport {
    pub ops_replayed: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub gaps: Vec<MirrorGap>,
    pub duplicates: u64,
}

/// The applier resolves belief-axis timestamps from `now`; replay each op at
/// its own recorded instant so the rebuilt log is identical.
fn replay_instant(op: &Op, fallback: i64) -> i64 {
    match op {
        Op::CreateEdge {
            recorded_at: Some(t),
            ..
        } => *t,
        Op::CloseEdge {
            superseded_at: Some(t),
            ..
        } => *t,
        _ => fallback,
    }
}

pub fn rebuild(
    wh: &Warehouse,
    out_db: &Path,
    spec: IndexSpec,
) -> Result<RebuildReport, WarehouseError> {
    if out_db.exists() {
        return Err(WarehouseError::Invalid(format!(
            "{} already exists",
            out_db.display()
        )));
    }
    let mut ops: Vec<(u64, i64, Op)> = Vec::new();
    for ev in wh.events()? {
        if ev.kind != Kind::Op {
            continue;
        }
        let Some(o) = ev.op else { continue };
        let op: Op = serde_json::from_value(o.body)
            .map_err(|e| WarehouseError::Invalid(format!("op seq {}: {e}", o.seq)))?;
        ops.push((o.seq, ev.ts, op));
    }
    ops.sort_by_key(|(seq, _, _)| *seq);
    let mut rep = RebuildReport::default();
    let mut deduped: Vec<(u64, i64, Op)> = Vec::with_capacity(ops.len());
    for item in ops {
        if deduped.last().is_some_and(|(s, _, _)| *s == item.0) {
            rep.duplicates += 1;
        } else {
            deduped.push(item);
        }
    }
    if deduped.is_empty() {
        return Err(WarehouseError::Invalid("no op events to replay".into()));
    }
    rep.first_seq = deduped[0].0;
    rep.last_seq = deduped.last().expect("non-empty").0;
    let mut expected = rep.first_seq;
    for (seq, _, _) in &deduped {
        if *seq > expected {
            rep.gaps.push(MirrorGap {
                from: expected,
                to: seq - 1,
            });
        }
        expected = seq + 1;
    }
    let db = Db::open_with(out_db, spec)?;
    for (_, ts, op) in deduped {
        let at = replay_instant(&op, ts);
        db.submit_at(vec![op], at)?;
        rep.ops_replayed += 1;
    }
    Ok(rep)
}
