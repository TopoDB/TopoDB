//! Unit: OKF `note` frontmatter parse/serialize. Unlike the obsidian mapper,
//! OKF frontmatter is NESTED (`generated: {by, at}`, `verified: [{...}]`,
//! `sources: [{...}]`), so the `Note` parser must accept and round-trip nested
//! YAML mappings/sequences byte-for-byte (design §"Why a new crate", point 2).

use serde_yaml::Value as Yaml;
use topodb_okf::Note;

fn y(s: &str) -> Yaml {
    Yaml::String(s.into())
}

#[test]
fn parses_nested_frontmatter_and_body() {
    let src = concat!(
        "---\n",
        "type: metric\n",
        "title: Revenue\n",
        "generated:\n",
        "  by: openwiki/0.3\n",
        "  at: \"2026-08-01T00:00:00Z\"\n",
        "---\n",
        "Body line.\n",
    );
    let n = Note::parse(src).unwrap();
    assert_eq!(n.body, "Body line.\n");
    assert_eq!(
        n.frontmatter.get(y("type")).and_then(|v| v.as_str()),
        Some("metric")
    );
    let gen = n
        .frontmatter
        .get(y("generated"))
        .and_then(|v| v.as_mapping())
        .expect("generated must parse as a nested mapping");
    assert_eq!(
        gen.get(y("by")).and_then(|v| v.as_str()),
        Some("openwiki/0.3")
    );
}

#[test]
fn round_trip_preserves_nested_frontmatter() {
    let src = concat!(
        "---\n",
        "type: metric\n",
        "sources:\n",
        "- resource: https://example.com/report.pdf\n",
        "  author: analyst/1.0\n",
        "verified:\n",
        "- by: human:drew\n",
        "  at: \"2026-08-02T00:00:00Z\"\n",
        "---\n",
        "Prose body.\n",
    );
    let n = Note::parse(src).unwrap();
    // Re-parsing the serialized form must yield an equal note (semantic
    // round-trip; we own canonical serialization per design §Round-trip).
    let again = Note::parse(&n.serialize()).unwrap();
    assert_eq!(again.frontmatter, n.frontmatter);
    assert_eq!(again.body, "Prose body.\n");
    let srcs = again
        .frontmatter
        .get(y("sources"))
        .and_then(|v| v.as_sequence())
        .expect("sources is a list");
    assert_eq!(srcs.len(), 1);
}

#[test]
fn no_frontmatter_is_all_body() {
    let n = Note::parse("# Onboarding Guide\n\nSteps.\n").unwrap();
    assert!(n.frontmatter.is_empty());
    assert_eq!(n.body, "# Onboarding Guide\n\nSteps.\n");
}

#[test]
fn id_stamp_round_trips() {
    let mut n = Note::parse("---\ntype: metric\n---\nx\n").unwrap();
    assert_eq!(n.id(), None);
    n.set_id("01J9Z6S3V0AAAAAAAAAAAAAAAA");
    let again = Note::parse(&n.serialize()).unwrap();
    assert_eq!(again.id().as_deref(), Some("01J9Z6S3V0AAAAAAAAAAAAAAAA"));
    assert_eq!(again.body, "x\n");
}
