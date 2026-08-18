//! Segments: the ONE open JSONL file (append-only) plus sealed, lz4-framed,
//! blake3-hashed immutable files; `archive/` holds cold ones (spec §4).
use crate::event::{encode_line, parse_lines, Event, Kind};
use crate::manifest::{Manifest, SegmentEntry};
use crate::Layout;
use serde::Serialize;
use std::io::{Read, Write};
use std::path::PathBuf;

pub fn segment_path(layout: &Layout, e: &SegmentEntry) -> PathBuf {
    if !e.sealed {
        return layout.segments.join(format!("{}.jsonl", e.name));
    }
    let dir = if e.archived {
        &layout.archive
    } else {
        &layout.segments
    };
    dir.join(format!("{}.jsonl.lz4", e.name))
}

pub fn should_roll(e: &SegmentEntry, now_ms: i64, segment_mb: u64) -> bool {
    e.bytes > segment_mb.saturating_mul(1024 * 1024) || now_ms.div_euclid(86_400_000) != e.day
}

/// Appends to the open segment (creating one if none), fsyncs, updates the entry.
/// Does NOT save the manifest — callers batch that.
pub fn append_events(
    layout: &Layout,
    m: &mut Manifest,
    events: &[Event],
    now_ms: i64,
) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    if m.open_entry().is_none() {
        m.push_segment(SegmentEntry::new_open(
            format!("seg-{}", crate::event::new_ulid()),
            now_ms,
        ));
    }
    let entry = m.open_entry_mut().expect("just ensured");
    let path = layout.segments.join(format!("{}.jsonl", entry.name));
    let mut buf = String::new();
    for ev in events {
        buf.push_str(&encode_line(ev));
        if entry.events == 0 {
            entry.first_ts = ev.ts;
        }
        entry.first_ts = entry.first_ts.min(ev.ts);
        entry.last_ts = entry.last_ts.max(ev.ts);
        entry.events += 1;
        if let (Kind::Op, Some(op)) = (&ev.kind, &ev.op) {
            entry.op_seq_min = Some(entry.op_seq_min.map_or(op.seq, |s| s.min(op.seq)));
            entry.op_seq_max = Some(entry.op_seq_max.map_or(op.seq, |s| s.max(op.seq)));
        }
    }
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(buf.as_bytes())?;
        f.sync_all()?;
    }
    entry.bytes += buf.len() as u64;
    Ok(())
}

/// Seals the open segment: lz4-frame compress, blake3 the compressed file,
/// mark sealed, delete the raw file. Returns the sealed name (None if nothing open).
pub fn seal_open(layout: &Layout, m: &mut Manifest) -> std::io::Result<Option<String>> {
    let Some(entry) = m.open_entry_mut() else {
        return Ok(None);
    };
    let raw = layout.segments.join(format!("{}.jsonl", entry.name));
    let bytes = if raw.is_file() {
        std::fs::read(&raw)?
    } else {
        Vec::new()
    };
    let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
    enc.write_all(&bytes)?;
    let compressed = enc.finish().map_err(std::io::Error::other)?;
    let sealed = layout.segments.join(format!("{}.jsonl.lz4", entry.name));
    let tmp = layout.segments.join(format!(
        "{}.jsonl.lz4.{}.tmp",
        entry.name,
        std::process::id()
    ));
    std::fs::write(&tmp, &compressed)?;
    std::fs::rename(&tmp, &sealed)?;
    entry.blake3 = Some(crate::blob::hash_hex(&compressed));
    entry.sealed = true;
    entry.bytes = compressed.len() as u64;
    let name = entry.name.clone();
    if raw.is_file() {
        std::fs::remove_file(&raw)?;
    }
    Ok(Some(name))
}

pub fn read_segment(layout: &Layout, e: &SegmentEntry) -> std::io::Result<(Vec<Event>, usize)> {
    let p = segment_path(layout, e);
    if !p.is_file() {
        return Ok((Vec::new(), 0));
    }
    let text = if e.sealed {
        let f = std::fs::File::open(&p)?;
        let mut dec = lz4_flex::frame::FrameDecoder::new(f);
        let mut s = String::new();
        dec.read_to_string(&mut s)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        s
    } else {
        std::fs::read_to_string(&p)?
    };
    Ok(parse_lines(&text))
}

/// Every event across every segment, manifest order (segments are appended
/// chronologically), sealed and open alike. Expired segments still contribute
/// their op/marker lines.
pub fn all_events(layout: &Layout, m: &Manifest) -> std::io::Result<Vec<Event>> {
    let mut out = Vec::new();
    for e in &m.segments {
        if e.deleted_at.is_some() {
            continue;
        }
        let (mut evs, _) = read_segment(layout, e)?;
        out.append(&mut evs);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerifyProblem {
    pub segment: String,
    pub reason: String,
}

/// Re-hashes every sealed segment against MANIFEST; reports missing files too.
pub fn verify(layout: &Layout, m: &Manifest) -> Vec<VerifyProblem> {
    let mut out = Vec::new();
    for e in m
        .segments
        .iter()
        .filter(|e| e.sealed && e.deleted_at.is_none())
    {
        let p = segment_path(layout, e);
        match std::fs::read(&p) {
            Err(err) => out.push(VerifyProblem {
                segment: e.name.clone(),
                reason: format!("unreadable: {err}"),
            }),
            Ok(bytes) => {
                let h = crate::blob::hash_hex(&bytes);
                if Some(&h) != e.blake3.as_ref() {
                    out.push(VerifyProblem {
                        segment: e.name.clone(),
                        reason: format!("hash mismatch: manifest {:?}, file {h}", e.blake3),
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;
    use crate::manifest::Manifest;
    use crate::Layout;

    fn ev(ts: i64) -> Event {
        Event {
            id: new_ulid(),
            ts,
            host: "h".into(),
            kind: Kind::Marker,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            op: None,
            marker: Some(Marker {
                kind: MarkerType::SessionStart,
                harness: "t".into(),
                session: "s".into(),
                scope: "shared".into(),
                node_ids: vec![],
            }),
        }
    }
    fn op(ts: i64, seq: u64) -> Event {
        Event {
            id: new_ulid(),
            ts,
            host: "h".into(),
            kind: Kind::Op,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            op: Some(OpEvent {
                seq,
                body: serde_json::json!({"RemoveNode":{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}}),
            }),
            marker: None,
        }
    }
    fn setup() -> (tempfile::TempDir, Layout, Manifest) {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(t.path().join("w"));
        l.ensure().unwrap();
        let m = Manifest::load_or_init(&l).unwrap();
        (t, l, m)
    }

    #[test]
    fn append_creates_open_segment_and_tracks_counts() {
        let (_t, l, mut m) = setup();
        append_events(&l, &mut m, &[ev(10), op(11, 5), op(12, 6)], 12).unwrap();
        let e = m.open_entry().unwrap().clone();
        assert_eq!(e.events, 3);
        assert_eq!((e.first_ts, e.last_ts), (10, 12));
        assert_eq!((e.op_seq_min, e.op_seq_max), (Some(5), Some(6)));
        assert!(segment_path(&l, &e).ends_with(format!("{}.jsonl", e.name)));
        let (evs, bad) = read_segment(&l, &e).unwrap();
        assert_eq!((evs.len(), bad), (3, 0));
        // second append reuses the same open segment
        append_events(&l, &mut m, &[ev(13)], 13).unwrap();
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.open_entry().unwrap().events, 4);
    }

    #[test]
    fn seal_compresses_hashes_and_reads_back() {
        let (_t, l, mut m) = setup();
        append_events(&l, &mut m, &[ev(1), ev(2)], 2).unwrap();
        let name = seal_open(&l, &mut m).unwrap().unwrap();
        let e = m.segments.iter().find(|s| s.name == name).unwrap().clone();
        assert!(e.sealed);
        assert!(e.blake3.as_deref().unwrap().starts_with("b3:"));
        assert!(segment_path(&l, &e).ends_with(format!("{name}.jsonl.lz4")));
        assert!(!l.segments.join(format!("{name}.jsonl")).exists());
        let (evs, _) = read_segment(&l, &e).unwrap();
        assert_eq!(evs.len(), 2);
        assert!(m.open_entry().is_none());
        assert!(seal_open(&l, &mut m).unwrap().is_none()); // nothing open
        assert!(verify(&l, &m).is_empty());
    }

    #[test]
    fn should_roll_on_size_or_day() {
        let mut e = crate::manifest::SegmentEntry::new_open("seg-X".into(), 0);
        assert!(!should_roll(&e, 1000, 64));
        e.bytes = 65 * 1024 * 1024;
        assert!(should_roll(&e, 1000, 64));
        e.bytes = 0;
        assert!(should_roll(&e, 86_400_000 + 1, 64)); // next UTC day
    }

    #[test]
    fn all_events_spans_sealed_and_open_in_order_and_verify_detects_tamper() {
        let (_t, l, mut m) = setup();
        append_events(&l, &mut m, &[ev(1)], 1).unwrap();
        seal_open(&l, &mut m).unwrap();
        append_events(&l, &mut m, &[ev(2), ev(3)], 3).unwrap();
        let all = all_events(&l, &m).unwrap();
        assert_eq!(all.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![1, 2, 3]);
        // tamper the sealed file
        let sealed = m.segments.iter().find(|s| s.sealed).unwrap().clone();
        std::fs::write(segment_path(&l, &sealed), b"junk").unwrap();
        let problems = verify(&l, &m);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].segment, sealed.name);
    }
}
