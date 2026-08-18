//! Tiering (spec §8): hot -> warm -> cold -> expired; monotone, idempotent, bounded.
use crate::derive::{ARTIFACT_LABEL, HAS_CHUNK_EDGE, TIER_PROP};
use crate::event::encode_line;
use crate::manifest::Tier;
use crate::segment::{read_segment, segment_path};
use crate::{Warehouse, WarehouseError};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use topodb::{Db, NodeRecord, Op, PropValue, Scope, TimeAxis};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TierReport {
    pub to_warm: usize,
    pub to_cold: usize,
    pub to_expired: usize,
    pub purged: usize,
    pub segments_archived: usize,
    pub segments_stripped: usize,
}

const DAY_MS: i64 = 86_400_000;
fn rank(t: &str) -> u8 {
    match t {
        "hot" => 0,
        "warm" => 1,
        "cold" => 2,
        _ => 3,
    }
}
fn tier_of(n: &NodeRecord) -> String {
    match n.props.get(TIER_PROP) {
        Some(PropValue::Str(s)) => s.clone(),
        _ => "hot".into(),
    }
}
fn last_seen(n: &NodeRecord) -> i64 {
    match n.props.get("last_seen") {
        Some(PropValue::DateTime(t)) | Some(PropValue::Int(t)) => *t,
        _ => 0,
    }
}
fn set_tier(id: topodb::NodeId, t: &str) -> Op {
    let mut p = BTreeMap::new();
    p.insert(TIER_PROP.to_string(), Some(PropValue::Str(t.to_string())));
    Op::SetNodeProps { id, props: p }
}

pub fn tier(db: &Db, wh: &mut Warehouse, now_ms: i64) -> Result<TierReport, WarehouseError> {
    let cfg = wh.config.clone();
    let mut rep = TierReport::default();
    let mut scopes: Vec<Scope> = wh
        .manifest
        .scopes
        .iter()
        .map(|x| topodb_json::resolve_scope(Some(x), Scope::Shared).unwrap_or(Scope::Shared))
        .collect();
    scopes.push(Scope::Shared);
    let set = topodb_json::scopes_to_scope_set(&scopes);
    let mut arts = db.nodes_by_label_unbumped(&set, ARTIFACT_LABEL);
    arts.sort_by_key(last_seen);
    let mut handled = 0usize;
    for a in arts {
        if handled >= cfg.tier_batch {
            break;
        }
        let age_days = (now_ms - last_seen(&a)) / DAY_MS;
        let cur = tier_of(&a);
        let target = if age_days > cfg.retention_days as i64 {
            "expired"
        } else if age_days > cfg.warm_days as i64 {
            "cold"
        } else if age_days > cfg.hot_days as i64 {
            "warm"
        } else {
            "hot"
        };
        if rank(target) <= rank(&cur) {
            continue;
        }
        let aset = topodb_json::scopes_to_scope_set(&[a.scope, Scope::Shared]);
        let chunks = db.edges_from(
            &aset,
            a.id,
            None,
            Some(HAS_CHUNK_EDGE),
            true,
            TimeAxis::Valid,
        )?;
        let mut ops = Vec::new();
        if rank(target) >= 1 && rank(&cur) < 1 {
            // -> warm: strip text
            for e in &chunks {
                let mut p = BTreeMap::new();
                p.insert("text".to_string(), None);
                ops.push(Op::SetNodeProps { id: e.to, props: p });
            }
            if target == "warm" {
                rep.to_warm += 1;
            }
        }
        if rank(target) >= 2 {
            // -> cold: drop chunks
            for e in &chunks {
                ops.push(Op::RemoveNode { id: e.to });
            }
            if target == "cold" {
                rep.to_cold += 1;
            }
        }
        if target == "expired" {
            rep.to_expired += 1;
            if cfg.purge_expired {
                ops.push(Op::RemoveNode { id: a.id });
                rep.purged += 1;
            }
        }
        if !(target == "expired" && cfg.purge_expired) {
            ops.push(set_tier(a.id, target));
        }
        db.submit_at(ops, now_ms)?;
        handled += 1;
    }

    // segments
    let names: Vec<String> = wh
        .manifest
        .segments
        .iter()
        .filter(|e| e.sealed && e.deleted_at.is_none())
        .map(|e| e.name.clone())
        .collect();
    for name in names {
        let entry = wh.manifest.entry_mut(&name).expect("present").clone();
        let age_days = (now_ms - entry.last_ts) / DAY_MS;
        if age_days > cfg.retention_days as i64 && entry.tier != Tier::Expired {
            // strip artifact bodies, keep everything else; rewrite in place (archived or not)
            let (evs, _) = read_segment(&wh.layout, &entry)?;
            let mut buf = String::new();
            for mut ev in evs {
                if let Some(a) = ev.artifact.as_mut() {
                    a.content = None;
                    a.blob = None;
                }
                buf.push_str(&encode_line(&ev));
            }
            let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
            enc.write_all(buf.as_bytes())?;
            let compressed = enc.finish().map_err(std::io::Error::other)?;
            let target_entry = {
                let mut e = entry.clone();
                e.archived = true;
                e
            };
            let path = segment_path(&wh.layout, &target_entry);
            let tmp = path.with_extension(format!("lz4.{}.tmp", std::process::id()));
            std::fs::write(&tmp, &compressed)?;
            std::fs::rename(&tmp, &path)?;
            let old_path = segment_path(&wh.layout, &entry);
            if old_path != path && old_path.is_file() {
                std::fs::remove_file(&old_path)?;
            }
            let e = wh.manifest.entry_mut(&name).expect("present");
            e.original_blake3 = e.original_blake3.clone().or(e.blake3.clone());
            e.blake3 = Some(crate::blob::hash_hex(&compressed));
            e.bytes = compressed.len() as u64;
            e.archived = true;
            e.tier = Tier::Expired;
            rep.segments_stripped += 1;
        } else if age_days > cfg.warm_days as i64 && !entry.archived {
            let from = segment_path(&wh.layout, &entry);
            let to_entry = {
                let mut e = entry.clone();
                e.archived = true;
                e
            };
            let to = segment_path(&wh.layout, &to_entry);
            if from.is_file() {
                std::fs::rename(&from, &to)?;
            }
            let e = wh.manifest.entry_mut(&name).expect("present");
            e.archived = true;
            e.tier = Tier::Cold;
            rep.segments_archived += 1;
        }
    }
    wh.save()?;
    crate::report::set_last_run(db, "tier", now_ms)?;
    Ok(rep)
}
