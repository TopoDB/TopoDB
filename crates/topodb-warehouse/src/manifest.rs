//! MANIFEST.json: the warehouse's index of segments, mirror gaps, recent
//! event ids (drain idempotence), and scopes seen. Written atomically.
use crate::Layout;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

pub const MANIFEST_VERSION: u32 = 1;
pub const RECENT_IDS_CAP: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hot,
    Warm,
    Cold,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorGap {
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentEntry {
    /// Base name `seg-<ulid>` (file suffix depends on state — see `segment::segment_path`).
    pub name: String,
    #[serde(default)]
    pub blake3: Option<String>,
    #[serde(default)]
    pub original_blake3: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    /// UTC day (ts / 86_400_000) the segment was opened on — day-boundary roll.
    pub day: i64,
    #[serde(default)]
    pub op_seq_min: Option<u64>,
    #[serde(default)]
    pub op_seq_max: Option<u64>,
    #[serde(default)]
    pub events: u64,
    #[serde(default)]
    pub bytes: u64,
    pub sealed: bool,
    pub tier: Tier,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub deleted_at: Option<i64>,
}
impl SegmentEntry {
    pub fn new_open(name: String, now_ms: i64) -> Self {
        SegmentEntry {
            name,
            blake3: None,
            original_blake3: None,
            first_ts: now_ms,
            last_ts: now_ms,
            day: now_ms.div_euclid(86_400_000),
            op_seq_min: None,
            op_seq_max: None,
            events: 0,
            bytes: 0,
            sealed: false,
            tier: Tier::Hot,
            archived: false,
            deleted_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub host_id: String,
    #[serde(default)]
    pub segments: Vec<SegmentEntry>,
    #[serde(default)]
    pub gaps: Vec<MirrorGap>,
    #[serde(default)]
    pub recent_ids: Vec<String>,
    #[serde(default)]
    pub scopes: BTreeSet<String>,
    #[serde(skip)]
    recent_set: HashSet<String>,
}

impl Manifest {
    pub fn new_with_host(host_id: String) -> Self {
        Manifest {
            format_version: MANIFEST_VERSION,
            host_id,
            segments: vec![],
            gaps: vec![],
            recent_ids: vec![],
            scopes: BTreeSet::new(),
            recent_set: HashSet::new(),
        }
    }
    pub fn load_or_init(layout: &Layout) -> std::io::Result<Self> {
        let p = layout.manifest_path();
        if p.is_file() {
            let text = std::fs::read_to_string(&p)?;
            let mut m: Manifest = serde_json::from_str(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            m.recent_set = m.recent_ids.iter().cloned().collect();
            Ok(m)
        } else {
            let m = Manifest::new_with_host(crate::event::new_ulid());
            m.save(layout)?;
            Ok(m)
        }
    }
    /// tmp + rename; the tmp lives in the same dir so the rename is atomic.
    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        let p = layout.manifest_path();
        let tmp = layout
            .root
            .join(format!("MANIFEST.json.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(self).expect("manifest serializes");
        std::fs::write(&tmp, text)?;
        match std::fs::rename(&tmp, &p) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
    /// Records `id`; returns false if it was already recorded (duplicate).
    pub fn note_id(&mut self, id: &str) -> bool {
        if self.recent_set.contains(id) {
            return false;
        }
        self.recent_ids.push(id.to_string());
        self.recent_set.insert(id.to_string());
        while self.recent_ids.len() > RECENT_IDS_CAP {
            let old = self.recent_ids.remove(0);
            self.recent_set.remove(&old);
        }
        true
    }
    pub fn note_scope(&mut self, scope: &str) {
        self.scopes.insert(scope.to_string());
    }
    pub fn open_entry(&self) -> Option<&SegmentEntry> {
        self.segments.iter().find(|s| !s.sealed)
    }
    pub fn open_entry_mut(&mut self) -> Option<&mut SegmentEntry> {
        self.segments.iter_mut().find(|s| !s.sealed)
    }
    pub fn push_segment(&mut self, e: SegmentEntry) {
        self.segments.push(e);
    }
    pub fn entry_mut(&mut self, name: &str) -> Option<&mut SegmentEntry> {
        self.segments.iter_mut().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layout;

    #[test]
    fn init_creates_host_id_and_persists() {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(t.path().join("w"));
        l.ensure().unwrap();
        let m = Manifest::load_or_init(&l).unwrap();
        assert_eq!(m.host_id.len(), 26);
        assert_eq!(m.format_version, MANIFEST_VERSION);
        m.save(&l).unwrap();
        let m2 = Manifest::load_or_init(&l).unwrap();
        assert_eq!(m2.host_id, m.host_id);
        assert!(l.manifest_path().is_file());
        assert!(!l.root.join("MANIFEST.json.tmp").exists());
    }

    #[test]
    fn note_id_dedups_and_is_bounded() {
        let mut m = Manifest::new_with_host("h".into());
        assert!(m.note_id("a"));
        assert!(!m.note_id("a"));
        for i in 0..RECENT_IDS_CAP + 5 {
            m.note_id(&format!("id{i}"));
        }
        assert!(m.recent_ids.len() <= RECENT_IDS_CAP);
        assert!(!m.recent_ids.contains(&"a".to_string())); // evicted
        assert!(m.note_id("a")); // forgotten => accepted again
    }

    #[test]
    fn open_entry_is_the_single_unsealed_segment() {
        let mut m = Manifest::new_with_host("h".into());
        assert!(m.open_entry().is_none());
        m.push_segment(SegmentEntry::new_open("seg-A".into(), 100));
        assert_eq!(m.open_entry().unwrap().name, "seg-A");
        m.entry_mut("seg-A").unwrap().sealed = true;
        assert!(m.open_entry().is_none());
    }

    #[test]
    fn scopes_are_a_set() {
        let mut m = Manifest::new_with_host("h".into());
        m.note_scope("shared");
        m.note_scope("shared");
        m.note_scope("01A");
        assert_eq!(m.scopes.len(), 2);
    }
}
