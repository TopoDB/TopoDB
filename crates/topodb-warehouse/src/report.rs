//! `status`: what the warehouse holds, per tier, plus mirror/spool health.
use crate::manifest::Tier;
use crate::{Warehouse, WarehouseError};
use serde::Serialize;
use std::collections::BTreeMap;
use topodb::Db;

pub const LAST_RUN_KEYS: [(&str, &str); 3] = [
    ("drain", "warehouse:last:drain"),
    ("derive", "warehouse:last:derive"),
    ("tier", "warehouse:last:tier"),
];
pub fn set_last_run(db: &Db, which: &str, now_ms: i64) -> Result<(), topodb::TopoError> {
    let key = LAST_RUN_KEYS
        .iter()
        .find(|(w, _)| *w == which)
        .map(|(_, k)| *k)
        .unwrap_or_else(|| panic!("unknown last-run key {which}"));
    db.set_meta(key, now_ms.to_string().as_bytes())
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub path: String,
    pub host_id: String,
    pub segments: usize,
    pub sealed: usize,
    pub archived: usize,
    pub expired: usize,
    pub open_segment_bytes: u64,
    pub segment_bytes: u64,
    pub segment_tiers: BTreeMap<String, usize>,
    pub spool_files: usize,
    pub spool_bytes: u64,
    pub blobs: usize,
    pub blob_bytes: u64,
    pub events: u64,
    pub gaps: usize,
    pub scopes: Vec<String>,
    pub mirrored_seq: Option<u64>,
    pub current_seq: Option<u64>,
    pub last_run: BTreeMap<String, Option<i64>>,
}

fn dir_stats(p: &std::path::Path) -> (usize, u64) {
    let mut n = 0;
    let mut b = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                let (n2, b2) = dir_stats(&path);
                n += n2;
                b += b2;
            } else if let Ok(md) = e.metadata() {
                n += 1;
                b += md.len();
            }
        }
    }
    (n, b)
}

impl Warehouse {
    pub fn status(&self, db: Option<&Db>) -> Result<Status, WarehouseError> {
        let m = &self.manifest;
        let mut tiers = BTreeMap::new();
        for e in &m.segments {
            let k = match e.tier {
                Tier::Hot => "hot",
                Tier::Warm => "warm",
                Tier::Cold => "cold",
                Tier::Expired => "expired",
            };
            *tiers.entry(k.to_string()).or_insert(0) += 1;
        }
        let (spool_files, spool_bytes) = dir_stats(&self.layout.spool);
        let (blobs, blob_bytes) = dir_stats(&self.layout.blobs);
        let mut last_run = BTreeMap::new();
        let (mut mirrored_seq, mut current_seq) = (None, None);
        if let Some(db) = db {
            mirrored_seq = Some(crate::mirror::mirrored_seq(db)?);
            current_seq = Some(db.current_seq()?);
            for (w, k) in LAST_RUN_KEYS {
                let v = db.get_meta(k)?.and_then(|b| {
                    std::str::from_utf8(&b)
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                });
                last_run.insert(w.to_string(), v);
            }
        }
        Ok(Status {
            path: self.layout.root.display().to_string(),
            host_id: m.host_id.clone(),
            segments: m.segments.len(),
            sealed: m.segments.iter().filter(|e| e.sealed).count(),
            archived: m.segments.iter().filter(|e| e.archived).count(),
            expired: m
                .segments
                .iter()
                .filter(|e| e.tier == Tier::Expired)
                .count(),
            open_segment_bytes: m.open_entry().map_or(0, |e| e.bytes),
            segment_bytes: m.segments.iter().map(|e| e.bytes).sum(),
            segment_tiers: tiers,
            spool_files,
            spool_bytes,
            blobs,
            blob_bytes,
            events: m.segments.iter().map(|e| e.events).sum(),
            gaps: m.gaps.len(),
            scopes: m.scopes.iter().cloned().collect(),
            mirrored_seq,
            current_seq,
            last_run,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Warehouse, WarehouseConfig};
    #[test]
    fn status_counts_segments_spool_blobs_and_seqs() {
        let t = tempfile::tempdir().unwrap();
        let db =
            topodb::Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let mut wh = Warehouse::open(
            &t.path().join("w"),
            WarehouseConfig {
                spool_min_age_ms: 0,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(wh.layout.spool.join("x.jsonl"), "{}\n").unwrap(); // one bad line, still counted as backlog
        let s0 = wh.status(Some(&db)).unwrap();
        assert_eq!(
            (s0.spool_files, s0.segments, s0.mirrored_seq, s0.current_seq),
            (1, 0, Some(0), Some(0))
        );
        wh.drain(1).unwrap();
        wh.mirror(&db, 2).unwrap();
        let s = wh.status(None).unwrap();
        assert_eq!((s.spool_files, s.mirrored_seq), (0, None));
        assert_eq!(s.host_id, wh.manifest.host_id);
        let j = serde_json::to_value(&s).unwrap();
        assert!(j.get("open_segment_bytes").is_some());
    }
}
