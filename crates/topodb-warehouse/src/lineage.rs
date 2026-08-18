//! `evidence` rule (spec §7): for each memory_write marker of session S at t,
//! evidence = distinct artifacts of S in (t_prev, t], most recent K, where
//! t_prev = S's previous memory_write, else session_start, else -inf.
use crate::event::{Event, Kind, MarkerType};
use std::collections::HashMap;

pub const EVIDENCE_RULE: &str = "turn-window/1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidencePick {
    pub memory_ids: Vec<String>,
    pub session: String,
    pub scope: String,
    pub ts: i64,
    /// (scope, hash), oldest first, at most K, distinct by (scope, hash) keeping the LATEST sighting's position.
    pub artifacts: Vec<(String, String)>,
}

pub fn evidence_windows(events: &[Event], k: usize) -> Vec<EvidencePick> {
    // group by session, preserving (ts, id) order
    let mut by_session: HashMap<&str, Vec<&Event>> = HashMap::new();
    for ev in events {
        let session = match (&ev.kind, &ev.source, &ev.marker) {
            (Kind::Artifact, Some(s), _) => s.session.as_str(),
            (Kind::Marker, _, Some(m)) => m.session.as_str(),
            _ => continue,
        };
        by_session.entry(session).or_default().push(ev);
    }
    let mut out = Vec::new();
    let mut sessions: Vec<&str> = by_session.keys().copied().collect();
    sessions.sort_unstable();
    for s in sessions {
        let mut evs = by_session.remove(s).expect("present");
        evs.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        let mut window_start: Option<i64> = None; // exclusive lower bound; None = -inf
        let mut since_start: Vec<(i64, String, String)> = Vec::new(); // (ts, scope, hash) after window_start
        for ev in evs {
            match (&ev.kind, &ev.artifact, &ev.marker) {
                (Kind::Artifact, Some(a), _) => {
                    if let (Some(h), Some(src)) = (&a.hash, &ev.source) {
                        if window_start.is_none_or(|w| ev.ts > w) {
                            since_start.push((ev.ts, src.scope.clone(), h.clone()));
                        }
                    }
                }
                (Kind::Marker, _, Some(m)) => match m.kind {
                    MarkerType::SessionStart => {
                        window_start = Some(ev.ts);
                        since_start.clear();
                    }
                    MarkerType::SessionEnd => {}
                    MarkerType::MemoryWrite => {
                        if !m.node_ids.is_empty() {
                            // distinct by (scope,hash) keeping the latest sighting, then most recent K, oldest-first
                            let mut latest: Vec<(i64, String, String)> = Vec::new();
                            for (ts, sc, h) in since_start.iter().filter(|(ts, _, _)| *ts <= ev.ts)
                            {
                                if let Some(e) =
                                    latest.iter_mut().find(|(_, s2, h2)| s2 == sc && h2 == h)
                                {
                                    e.0 = *ts;
                                } else {
                                    latest.push((*ts, sc.clone(), h.clone()));
                                }
                            }
                            latest.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
                            let start = latest.len().saturating_sub(k);
                            let artifacts = latest[start..]
                                .iter()
                                .map(|(_, s2, h)| (s2.clone(), h.clone()))
                                .collect();
                            out.push(EvidencePick {
                                memory_ids: m.node_ids.clone(),
                                session: s.to_string(),
                                scope: m.scope.clone(),
                                ts: ev.ts,
                                artifacts,
                            });
                        }
                        window_start = Some(ev.ts);
                        since_start.clear();
                    }
                },
                _ => {}
            }
        }
    }
    out.sort_by(|a, b| (a.ts, &a.session).cmp(&(b.ts, &b.session)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;

    fn art(ts: i64, session: &str, hash: &str) -> Event {
        Event {
            id: format!("a{ts}"),
            ts,
            host: "h".into(),
            kind: Kind::Artifact,
            v: EVENT_VERSION,
            source: Some(Source {
                harness: "cc".into(),
                session: session.into(),
                scope: "S".into(),
                turn: None,
                tool: None,
                cwd: None,
                agent: None,
            }),
            artifact: Some(Artifact {
                kind: ArtifactType::FileRead,
                locator: "f".into(),
                bytes: 1,
                hash: Some(hash.into()),
                redacted: false,
                redactions: vec![],
                redact_v: None,
                content: None,
                blob: None,
            }),
            op: None,
            marker: None,
        }
    }

    fn mk(ts: i64, session: &str, kind: MarkerType, ids: &[&str]) -> Event {
        Event {
            id: format!("m{ts}"),
            ts,
            host: "h".into(),
            kind: Kind::Marker,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            op: None,
            marker: Some(Marker {
                kind,
                harness: "cc".into(),
                session: session.into(),
                scope: "S".into(),
                node_ids: ids.iter().map(|s| s.to_string()).collect(),
            }),
        }
    }

    #[test]
    fn window_is_since_previous_memory_write_or_session_start() {
        let evs = vec![
            art(1, "s", "h0"), // before session_start: excluded
            mk(2, "s", MarkerType::SessionStart, &[]),
            art(3, "s", "h1"),
            art(4, "s", "h2"),
            mk(5, "s", MarkerType::MemoryWrite, &["M1"]),
            art(6, "s", "h3"),
            art(7, "s", "h2"), // h2 seen again after M1
            mk(8, "s", MarkerType::MemoryWrite, &["M2", "M3"]),
            art(9, "other", "h9"), // other session, ignored
            mk(10, "other", MarkerType::MemoryWrite, &["M9"]),
        ];
        let picks = evidence_windows(&evs, 20);
        assert_eq!(picks.len(), 3);
        assert_eq!(picks[0].memory_ids, vec!["M1"]);
        assert_eq!(
            picks[0].artifacts,
            vec![
                ("S".to_string(), "h1".to_string()),
                ("S".to_string(), "h2".to_string())
            ]
        );
        assert_eq!(picks[1].memory_ids, vec!["M2", "M3"]);
        assert_eq!(
            picks[1].artifacts,
            vec![
                ("S".to_string(), "h3".to_string()),
                ("S".to_string(), "h2".to_string())
            ]
        );
        assert_eq!(
            picks[2].artifacts,
            vec![("S".to_string(), "h9".to_string())]
        ); // no session_start: window from -inf
    }

    #[test]
    fn k_cap_keeps_most_recent_and_dedups_hashes() {
        let mut evs = vec![mk(0, "s", MarkerType::SessionStart, &[])];
        for i in 1..=30 {
            evs.push(art(i, "s", &format!("h{}", i % 25)));
        } // 30 sightings, 25 distinct
        evs.push(mk(31, "s", MarkerType::MemoryWrite, &["M"]));
        let p = evidence_windows(&evs, 5);
        assert_eq!(p[0].artifacts.len(), 5);
        assert_eq!(p[0].artifacts.last().unwrap().1, "h5"); // ts 30 -> 30%25 = 5 is the most recent
    }

    #[test]
    fn markers_without_node_ids_or_artifacts_without_hash_are_ignored() {
        let mut a = art(1, "s", "h");
        a.artifact.as_mut().unwrap().hash = None;
        let evs = vec![
            a,
            mk(2, "s", MarkerType::MemoryWrite, &[]),
            mk(3, "s", MarkerType::MemoryWrite, &["M"]),
        ];
        let p = evidence_windows(&evs, 20);
        assert_eq!(p.len(), 1);
        assert!(p[0].artifacts.is_empty());
    }
}
