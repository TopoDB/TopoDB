//! Drain: spool/*.jsonl -> redact -> hash -> inline|blob|pointer -> open segment (spec §6.2).
use crate::blob::{hash_hex, put_blob};
use crate::event::{parse_lines, Event, Kind};
use crate::manifest::Manifest;
use crate::redact::{redact, REDACT_VERSION};
use crate::{Layout, WarehouseConfig};
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DrainReport {
    pub files: usize,
    pub events: usize,
    pub skipped_lines: usize,
    pub duplicates: usize,
    pub deferred_files: usize,
    pub blobs_written: usize,
}

fn file_age_ms(p: &std::path::Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(m)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Applies redaction + size policy to one artifact event in place. Returns
/// true if a blob was written.
fn prepare_artifact(
    layout: &Layout,
    cfg: &WarehouseConfig,
    ev: &mut Event,
) -> std::io::Result<bool> {
    let Some(a) = ev.artifact.as_mut() else {
        return Ok(false);
    };
    let mut wrote_blob = false;
    if let Some(content) = a.content.take() {
        let content = if cfg.redact {
            let o = redact(&content);
            if !o.redactions.is_empty() {
                a.redacted = true;
                a.redactions = o.redactions;
            }
            o.text
        } else {
            content
        };
        a.redact_v = Some(REDACT_VERSION);
        let bytes = content.as_bytes();
        a.bytes = bytes.len() as u64;
        a.hash = Some(hash_hex(bytes));
        if a.bytes > cfg.max_artifact_kb * 1024 {
            // pointer-only: hash + locator survive, body does not
        } else if a.bytes > cfg.max_inline_kb * 1024 {
            wrote_blob = put_blob(layout, a.hash.as_ref().expect("set above"), bytes)?;
            a.blob = a.hash.clone();
        } else {
            a.content = Some(content);
        }
    }
    Ok(wrote_blob)
}

pub fn drain(
    layout: &Layout,
    m: &mut Manifest,
    cfg: &WarehouseConfig,
    now_ms: i64,
) -> std::io::Result<DrainReport> {
    let mut rep = DrainReport::default();
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&layout.spool)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    names.sort();
    let mut batch: Vec<Event> = Vec::new();
    let mut consumed: Vec<std::path::PathBuf> = Vec::new();
    for p in names {
        if cfg.spool_min_age_ms > 0 && file_age_ms(&p).is_some_and(|age| age < cfg.spool_min_age_ms)
        {
            rep.deferred_files += 1;
            continue;
        }
        let text = std::fs::read_to_string(&p)?;
        let (evs, bad) = parse_lines(&text);
        rep.skipped_lines += bad;
        rep.files += 1;
        for mut ev in evs {
            if !m.note_id(&ev.id) {
                rep.duplicates += 1;
                continue;
            }
            if ev.host.is_empty() {
                ev.host = m.host_id.clone();
            }
            if let Some(s) = &ev.source {
                m.note_scope(&s.scope);
            }
            if let Some(mk) = &ev.marker {
                m.note_scope(&mk.scope);
            }
            if ev.kind == Kind::Artifact && prepare_artifact(layout, cfg, &mut ev)? {
                rep.blobs_written += 1;
            }
            batch.push(ev);
        }
        consumed.push(p);
    }
    batch.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
    rep.events = batch.len();
    crate::segment::append_events(layout, m, &batch, now_ms)?;
    m.save(layout)?; // ids + segment entry durable BEFORE the spool files go
    for p in consumed {
        let _ = std::fs::remove_file(p);
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use crate::event::*;
    use crate::{Warehouse, WarehouseConfig};

    fn cfg() -> WarehouseConfig {
        WarehouseConfig {
            spool_min_age_ms: 0,
            ..WarehouseConfig::default()
        }
    }
    fn art(id: &str, ts: i64, content: &str) -> Event {
        Event {
            id: id.into(),
            ts,
            host: String::new(),
            kind: Kind::Artifact,
            v: EVENT_VERSION,
            source: Some(Source {
                harness: "claude-code".into(),
                session: "s1".into(),
                scope: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                turn: None,
                tool: Some("Bash".into()),
                cwd: None,
                agent: None,
            }),
            artifact: Some(Artifact {
                kind: ArtifactType::Command,
                locator: "ls".into(),
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
    fn write_spool(wh: &Warehouse, name: &str, evs: &[Event]) {
        let mut s = String::new();
        for e in evs {
            s.push_str(&encode_line(e));
        }
        std::fs::write(wh.layout.spool.join(name), s).unwrap();
    }

    #[test]
    fn drain_redacts_hashes_appends_and_deletes_spool() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        write_spool(
            &wh,
            "s1-1.jsonl",
            &[
                art("01B", 20, "token AKIAIOSFODNN7EXAMPLE"),
                art("01A", 10, "plain"),
            ],
        );
        let rep = wh.drain(100).unwrap();
        assert_eq!(
            (rep.files, rep.events, rep.duplicates, rep.skipped_lines),
            (1, 2, 0, 0)
        );
        assert!(std::fs::read_dir(&wh.layout.spool)
            .unwrap()
            .next()
            .is_none());
        let evs = wh.events().unwrap();
        assert_eq!(
            evs.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["01A", "01B"]
        ); // sorted by (ts,id)
        let a = evs[1].artifact.as_ref().unwrap();
        assert!(a.content.as_deref().unwrap().contains("«redacted:aws:"));
        assert!(
            a.redacted && a.hash.as_deref().unwrap().starts_with("b3:") && a.redact_v == Some(1)
        );
        assert_eq!(a.bytes as usize, a.content.as_ref().unwrap().len());
        assert_eq!(evs[0].host, wh.manifest.host_id); // host filled from manifest
        assert!(wh.manifest.scopes.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
    }

    #[test]
    fn drain_is_idempotent_on_event_id_and_tolerates_bad_lines() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        write_spool(&wh, "a.jsonl", &[art("01A", 1, "x")]);
        wh.drain(5).unwrap();
        // same event re-spooled (crash-before-delete replay) + one garbage line
        let mut s = encode_line(&art("01A", 1, "x"));
        s.push_str("not json\n");
        std::fs::write(wh.layout.spool.join("b.jsonl"), s).unwrap();
        let rep = wh.drain(6).unwrap();
        assert_eq!((rep.events, rep.duplicates, rep.skipped_lines), (0, 1, 1));
        assert_eq!(wh.events().unwrap().len(), 1);
    }

    #[test]
    fn size_policy_inline_blob_pointer() {
        let t = tempfile::tempdir().unwrap();
        let c = WarehouseConfig {
            max_inline_kb: 1,
            max_artifact_kb: 4,
            spool_min_age_ms: 0,
            ..WarehouseConfig::default()
        };
        let mut wh = Warehouse::open(&t.path().join("w"), c).unwrap();
        let small = "s".repeat(500);
        let mid = "m".repeat(2048);
        let big = "b".repeat(8192);
        write_spool(
            &wh,
            "a.jsonl",
            &[
                art("01A", 1, &small),
                art("01B", 2, &mid),
                art("01C", 3, &big),
            ],
        );
        let rep = wh.drain(9).unwrap();
        assert_eq!(rep.blobs_written, 1);
        let evs = wh.events().unwrap();
        let a = |i: usize| evs[i].artifact.as_ref().unwrap();
        assert!(a(0).content.is_some() && a(0).blob.is_none());
        assert!(a(1).content.is_none() && a(1).blob.is_some());
        assert_eq!(
            crate::blob::get_blob(&wh.layout, a(1).blob.as_ref().unwrap())
                .unwrap()
                .unwrap(),
            mid.as_bytes()
        );
        assert!(a(2).content.is_none() && a(2).blob.is_none() && a(2).hash.is_some()); // pointer-only
        assert_eq!(a(2).bytes, 8192);
    }

    #[test]
    fn young_spool_files_are_deferred() {
        let t = tempfile::tempdir().unwrap();
        let c = WarehouseConfig {
            spool_min_age_ms: 60_000,
            ..WarehouseConfig::default()
        };
        let mut wh = Warehouse::open(&t.path().join("w"), c).unwrap();
        write_spool(&wh, "a.jsonl", &[art("01A", 1, "x")]);
        let rep = wh.drain(1).unwrap();
        assert_eq!((rep.events, rep.deferred_files), (0, 1));
        assert!(wh.layout.spool.join("a.jsonl").is_file());
    }

    #[test]
    fn reopen_recovers_dedup_ids_from_open_segment_after_crash_before_manifest_save() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("w");
        {
            let mut wh = Warehouse::open(&dir, cfg()).unwrap();
            write_spool(&wh, "a.jsonl", &[art("01A", 1, "x")]);
            wh.drain(5).unwrap();
            // simulate a crash that happened after the segment append but before
            // the manifest save: forget the ids and persist that older state
            wh.manifest.recent_ids.clear();
            wh.manifest = {
                let mut m = crate::manifest::Manifest::new_with_host(wh.manifest.host_id.clone());
                m.segments = wh.manifest.segments.clone();
                m.scopes = wh.manifest.scopes.clone();
                m
            };
            wh.save().unwrap();
        }
        let mut wh = Warehouse::open(&dir, cfg()).unwrap();
        write_spool(&wh, "a-again.jsonl", &[art("01A", 1, "x")]); // the spool file survived the crash
        let rep = wh.drain(6).unwrap();
        assert_eq!((rep.events, rep.duplicates), (0, 1));
        assert_eq!(wh.events().unwrap().len(), 1);
    }
}
