//! Drain: spool/*.jsonl -> redact -> hash -> inline|blob|pointer -> open segment (spec §6.2).
use crate::blob::{hash_hex, put_blob};
use crate::event::{parse_lines, Event, Kind, Redaction};
use crate::manifest::Manifest;
use crate::redact::{redact, REDACT_VERSION};
use crate::{Layout, WarehouseConfig};
use serde::Serialize;

/// Merges `src` redaction counts into `dst`, aggregating by class (same
/// accounting as `redact::bump`) rather than replacing wholesale — needed
/// because `prepare_artifact` now redacts both `locator` and `content` into
/// the same `a.redactions` vec.
fn merge_redactions(dst: &mut Vec<Redaction>, src: Vec<Redaction>) {
    for r in src {
        if let Some(d) = dst.iter_mut().find(|d| d.class == r.class) {
            d.count += r.count;
        } else {
            dst.push(r);
        }
    }
}

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
    if cfg.redact {
        // Command artifacts carry the full command line (secrets and all) in
        // `locator`; redact it too, not just `content`.
        let o = redact(&a.locator);
        if !o.redactions.is_empty() {
            a.redacted = true;
            merge_redactions(&mut a.redactions, o.redactions);
        }
        a.locator = o.text;
    }
    if let Some(content) = a.content.take() {
        let content = if cfg.redact {
            let o = redact(&content);
            if !o.redactions.is_empty() {
                a.redacted = true;
                merge_redactions(&mut a.redactions, o.redactions);
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

/// Extension a spool file gets once a drain has claimed it.
const DRAINING_EXT: &str = "draining";

/// Claims a spool file by renaming it to a `.draining` name BEFORE it is
/// read. Writers append by path (pi keeps one `<session>-<pid>.jsonl` for the
/// whole session), so after the rename their next append recreates a fresh
/// `.jsonl` instead of landing in a file we are about to delete.
///
/// The target never collides with an existing file: `<name>.draining` taken
/// (a leftover from a drain that died between rename and `m.save`) →
/// `<name>.1.draining`, `<name>.2.draining`, … POSIX `rename` replaces its
/// destination, which would destroy a leftover's only on-disk copy before it
/// is durable; Windows fails the rename instead. Picking a free name makes
/// both platforms behave the same. Every `*.draining` file is drained first
/// on the next run; `Manifest::note_id` dedups anything already landed.
///
/// The `exists()` → `rename` probe is not atomic; it is safe because
/// warehouse mutation (drain included) is single-flighted by the db's
/// exclusive lock — every mutating path opens the db first (IMPROVEMENTS W8).
/// A second concurrent drainer would reintroduce a real race here.
fn claim(p: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let base = p.as_os_str().to_owned();
    let mut n: u32 = 0;
    loop {
        let mut name = base.clone();
        if n > 0 {
            name.push(format!(".{n}"));
        }
        name.push(".");
        name.push(DRAINING_EXT);
        let target = std::path::PathBuf::from(name);
        if target.exists() {
            n += 1;
            continue;
        }
        std::fs::rename(p, &target)?;
        return Ok(target);
    }
}

pub fn drain(
    layout: &Layout,
    m: &mut Manifest,
    cfg: &WarehouseConfig,
    now_ms: i64,
) -> std::io::Result<DrainReport> {
    let mut rep = DrainReport::default();
    let mut leftovers: Vec<std::path::PathBuf> = Vec::new();
    let mut names: Vec<std::path::PathBuf> = Vec::new();
    for p in std::fs::read_dir(&layout.spool)?.filter_map(|e| e.ok().map(|e| e.path())) {
        match p.extension().and_then(|x| x.to_str()) {
            Some("jsonl") => names.push(p),
            Some(DRAINING_EXT) => leftovers.push(p),
            _ => {}
        }
    }
    leftovers.sort();
    names.sort();
    let mut claimed: Vec<std::path::PathBuf> = leftovers;
    for p in names {
        if cfg.spool_min_age_ms > 0 && file_age_ms(&p).is_some_and(|age| age < cfg.spool_min_age_ms)
        {
            rep.deferred_files += 1;
            continue;
        }
        match claim(&p) {
            Ok(c) => claimed.push(c),
            // Windows sharing violation / another drainer got it first: not ours this run.
            Err(_) => rep.deferred_files += 1,
        }
    }
    let mut batch: Vec<Event> = Vec::new();
    let mut consumed: Vec<std::path::PathBuf> = Vec::new();
    for p in claimed {
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            // Unreadable (EACCES/EIO): leave it for a human, keep draining the rest.
            Err(_) => {
                rep.deferred_files += 1;
                continue;
            }
        };
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
    use super::claim;
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
    fn drain_redacts_the_artifact_locator_too() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        let mut ev = art("01A", 1, "ok\n");
        ev.artifact.as_mut().unwrap().locator = "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcDEFghiJKLmnoPQRstuVWXyz0123456789' https://x".into();
        write_spool(&wh, "s.jsonl", &[ev]);
        wh.drain(100).unwrap();
        let evs = wh.events().unwrap();
        let a = evs[0].artifact.as_ref().unwrap();
        assert!(
            a.locator.contains("«redacted:bearer:"),
            "locator: {}",
            a.locator
        );
        assert!(a.redacted);
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
    fn drain_recovers_draining_leftovers_and_claims_new_files() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        // A crash leftover: claimed by a previous drain that died before m.save.
        write_spool(&wh, "s1-1.jsonl.draining", &[art("01A", 1, "left")]);
        write_spool(&wh, "s1-1.jsonl", &[art("01B", 2, "fresh")]);
        let rep = wh.drain(100).unwrap();
        assert_eq!((rep.files, rep.events, rep.deferred_files), (2, 2, 0));
        assert!(std::fs::read_dir(&wh.layout.spool)
            .unwrap()
            .next()
            .is_none());
        let ids: Vec<String> = wh.events().unwrap().iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, vec!["01A".to_string(), "01B".to_string()]);
    }

    #[test]
    fn leftovers_drain_even_when_young_files_are_deferred() {
        let t = tempfile::tempdir().unwrap();
        let c = WarehouseConfig {
            spool_min_age_ms: 60_000,
            ..WarehouseConfig::default()
        };
        let mut wh = Warehouse::open(&t.path().join("w"), c).unwrap();
        write_spool(&wh, "a.jsonl.draining", &[art("01A", 1, "left")]);
        write_spool(&wh, "a.jsonl", &[art("01B", 2, "young")]);
        let rep = wh.drain(1).unwrap();
        assert_eq!((rep.events, rep.deferred_files), (1, 1));
        // The young writer file was neither claimed nor deleted; the leftover is gone.
        assert!(wh.layout.spool.join("a.jsonl").is_file());
        assert!(!wh.layout.spool.join("a.jsonl.draining").exists());
    }

    #[test]
    fn a_leftover_already_landed_is_deduped_not_double_counted() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        write_spool(&wh, "a.jsonl", &[art("01A", 1, "x")]);
        wh.drain(10).unwrap();
        // Simulate: the same events were claimed again by a drain that died after m.save.
        write_spool(&wh, "a.jsonl.draining", &[art("01A", 1, "x")]);
        let rep = wh.drain(20).unwrap();
        assert_eq!((rep.files, rep.events, rep.duplicates), (1, 0, 1));
        assert!(std::fs::read_dir(&wh.layout.spool)
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn claim_never_overwrites_an_existing_leftover() {
        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        write_spool(&wh, "a.jsonl.draining", &[art("01A", 1, "left")]);
        write_spool(&wh, "a.jsonl", &[art("01B", 2, "fresh")]);
        let c = claim(&wh.layout.spool.join("a.jsonl")).unwrap();
        assert_eq!(c, wh.layout.spool.join("a.jsonl.1.draining"));
        assert!(wh.layout.spool.join("a.jsonl.draining").is_file()); // leftover untouched on disk
        assert!(!wh.layout.spool.join("a.jsonl").exists());
        // A second fresh file claims the next free name.
        write_spool(&wh, "a.jsonl", &[art("01C", 3, "fresh2")]);
        assert_eq!(
            claim(&wh.layout.spool.join("a.jsonl")).unwrap(),
            wh.layout.spool.join("a.jsonl.2.draining")
        );
        // All three are leftovers now and drain together, in (ts, id) order.
        let rep = wh.drain(5).unwrap();
        assert_eq!((rep.files, rep.events, rep.deferred_files), (3, 3, 0));
        assert!(std::fs::read_dir(&wh.layout.spool)
            .unwrap()
            .next()
            .is_none());
        let ids: Vec<String> = wh.events().unwrap().iter().map(|e| e.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["01A".to_string(), "01B".to_string(), "01C".to_string()]
        );
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

    #[cfg(unix)]
    #[test]
    fn an_unreadable_leftover_is_deferred_not_fatal() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().unwrap();
        let mut wh = Warehouse::open(&t.path().join("w"), cfg()).unwrap();
        write_spool(&wh, "bad.jsonl.draining", &[art("01A", 1, "left")]);
        write_spool(&wh, "good.jsonl", &[art("01B", 2, "fresh")]);
        let bad = wh.layout.spool.join("bad.jsonl.draining");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root (or on a filesystem that ignores the mode bits, e.g.
        // some CI containers) makes the file readable anyway; in that case the
        // hardening path under test never triggers, so skip the assertions.
        if std::fs::read_to_string(&bad).is_ok() {
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }

        let rep = wh.drain(100).unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!((rep.files, rep.events, rep.deferred_files), (1, 1, 1));
        assert!(!wh.layout.spool.join("good.jsonl").exists());
        assert!(bad.is_file());
    }
}
