//! The one JSONL event schema (spec §5): artifact | op | marker.
use serde::{Deserialize, Serialize};

pub const EVENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Artifact,
    Op,
    Marker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    FileRead,
    Command,
    Diff,
    ToolOutput,
    TranscriptRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerType {
    SessionStart,
    SessionEnd,
    MemoryWrite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub harness: String,
    pub session: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redaction {
    pub class: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    #[serde(rename = "type")]
    pub kind: ArtifactType,
    pub locator: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<Redaction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact_v: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpEvent {
    pub seq: u64,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    #[serde(rename = "type")]
    pub kind: MarkerType,
    pub harness: String,
    pub session: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub ts: i64,
    #[serde(default)]
    pub host: String,
    pub kind: Kind,
    #[serde(default = "default_v")]
    pub v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<OpEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<Marker>,
}
fn default_v() -> u32 {
    EVENT_VERSION
}

/// One JSON object per line, newline-terminated.
pub fn encode_line(ev: &Event) -> String {
    let mut s = serde_json::to_string(ev).expect("event serializes");
    s.push('\n');
    s
}

/// Parse JSONL leniently: every unparseable line (garbage or a torn tail) is
/// counted in the second tuple slot and skipped — never fatal (spec §2).
pub fn parse_lines(text: &str) -> (Vec<Event>, usize) {
    let mut out = Vec::new();
    let mut bad = 0usize;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(ev) => out.push(ev),
            Err(_) => bad += 1,
        }
    }
    (out, bad)
}

pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact_event() -> Event {
        Event {
            id: new_ulid(),
            ts: 1_700_000_000_000,
            host: "h".into(),
            kind: Kind::Artifact,
            v: EVENT_VERSION,
            source: Some(Source {
                harness: "claude-code".into(),
                session: "s1".into(),
                scope: "shared".into(),
                turn: None,
                tool: Some("Read".into()),
                cwd: None,
                agent: None,
            }),
            artifact: Some(Artifact {
                kind: ArtifactType::FileRead,
                locator: "/a.rs".into(),
                bytes: 3,
                hash: None,
                redacted: false,
                redactions: vec![],
                redact_v: None,
                content: Some("abc".into()),
                blob: None,
            }),
            op: None,
            marker: None,
        }
    }

    #[test]
    fn roundtrip_artifact_line() {
        let ev = artifact_event();
        let line = encode_line(&ev);
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"kind\":\"artifact\""));
        assert!(line.contains("\"type\":\"file_read\""));
        let (evs, bad) = parse_lines(&line);
        assert_eq!(bad, 0);
        assert_eq!(evs, vec![ev]);
    }

    #[test]
    fn roundtrip_op_and_marker() {
        let op = Event {
            id: new_ulid(),
            ts: 1,
            host: "h".into(),
            kind: Kind::Op,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            op: Some(OpEvent {
                seq: 7,
                body: serde_json::json!({"RemoveNode":{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}}),
            }),
            marker: None,
        };
        let mk = Event {
            id: new_ulid(),
            ts: 2,
            host: "h".into(),
            kind: Kind::Marker,
            v: EVENT_VERSION,
            source: None,
            artifact: None,
            op: None,
            marker: Some(Marker {
                kind: MarkerType::MemoryWrite,
                harness: "claude-code".into(),
                session: "s".into(),
                scope: "shared".into(),
                node_ids: vec!["01ARZ3NDEKTSV4RRFFQ69G5FAV".into()],
            }),
        };
        let text = format!("{}{}", encode_line(&op), encode_line(&mk));
        let (evs, bad) = parse_lines(&text);
        assert_eq!(bad, 0);
        assert_eq!(evs, vec![op, mk]);
        assert!(text.contains("\"type\":\"memory_write\""));
    }

    #[test]
    fn torn_and_garbage_lines_are_counted_not_fatal() {
        let ev = artifact_event();
        let mut text = encode_line(&ev);
        text.push_str("{\"garbage\": tru\n");
        text.push_str(&encode_line(&ev)[..20]); // torn last line, no newline
        let (evs, bad) = parse_lines(&text);
        assert_eq!(evs.len(), 1);
        assert_eq!(bad, 2);
    }

    #[test]
    fn ulids_are_26_chars_and_unique() {
        let a = new_ulid();
        let b = new_ulid();
        assert_eq!(a.len(), 26);
        assert_ne!(a, b);
    }
}
