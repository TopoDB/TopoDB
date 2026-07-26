use crate::{extract_wikilinks, Note, ENTITY_STUB_KEY, RELATED_KEY, TOPODB_ID_KEY};
use serde_json::{json, Map, Value as Json};
use serde_yaml::Value as Yaml;
use std::collections::BTreeSet;
use topodb_json::entity_dedup_key;

#[derive(Debug)]
pub struct NoteInput {
    pub id: Option<String>,
    pub content: String,
    pub kind: Option<String>,
    pub props: Option<serde_json::Value>,
    pub entities: Vec<String>,
    pub is_entity_stub: bool,
}

pub fn note_to_input(note: &Note) -> Result<NoteInput, String> {
    let mut props = Map::new();
    let mut entities = extract_wikilinks(&note.body);
    let mut seen: BTreeSet<String> = entities.iter().map(|e| entity_dedup_key(e)).collect();
    let push_links = |text: &str, entities: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        for l in extract_wikilinks(text) {
            if seen.insert(entity_dedup_key(&l)) {
                entities.push(l);
            }
        }
    };
    let (mut id, mut kind, mut is_entity_stub) = (None, None, false);
    for (k, v) in &note.frontmatter {
        let key = k.as_str().ok_or("frontmatter keys must be strings")?;
        match key {
            TOPODB_ID_KEY => id = Some(v.as_str().ok_or("topodb-id must be a string")?.to_string()),
            "kind" => kind = Some(v.as_str().ok_or("kind must be a string")?.to_string()),
            ENTITY_STUB_KEY => is_entity_stub = v.as_bool().ok_or("entity must be a boolean")?,
            RELATED_KEY => match v {
                Yaml::String(s) => push_links(s, &mut entities, &mut seen),
                Yaml::Sequence(seq) => {
                    for item in seq {
                        let s = item.as_str().ok_or("related entries must be strings")?;
                        push_links(s, &mut entities, &mut seen);
                    }
                }
                _ => return Err("related must be a string or list of strings".into()),
            },
            _ => {
                let jv = yaml_to_json_scalar(key, v)?;
                if let Some(s) = jv.as_str() {
                    push_links(s, &mut entities, &mut seen);
                }
                props.insert(key.to_string(), jv);
            }
        }
    }
    Ok(NoteInput {
        id,
        content: note.body.trim_end().to_string(),
        kind,
        props: if props.is_empty() {
            None
        } else {
            Some(Json::Object(props))
        },
        entities,
        is_entity_stub,
    })
}

/// Scalars pass through; sequences of scalars flatten to "a, b, c"
/// (PropValue has no list type); anything nested is rejected.
fn yaml_to_json_scalar(key: &str, v: &Yaml) -> Result<Json, String> {
    match v {
        Yaml::String(s) => Ok(json!(s)),
        Yaml::Bool(b) => Ok(json!(b)),
        Yaml::Number(n) => {
            // Try integer first for fidelity (priority: 2 must be int, not 2.0)
            if let Some(i) = n.as_i64() {
                Ok(json!(i))
            } else if let Some(u) = n.as_u64() {
                Ok(json!(u))
            } else if let Some(f) = n.as_f64() {
                Ok(json!(f))
            } else {
                Err(format!("Number conversion failed for {key:?}"))
            }
        }
        Yaml::Null => Ok(json!("")),
        Yaml::Sequence(seq) => {
            let mut parts = Vec::new();
            for item in seq {
                match item {
                    Yaml::String(s) => parts.push(s.clone()),
                    Yaml::Bool(b) => parts.push(b.to_string()),
                    Yaml::Number(n) => parts.push(n.to_string()),
                    _ => return Err(format!("{key:?}: nested values are not supported")),
                }
            }
            Ok(json!(parts.join(", ")))
        }
        _ => Err(format!("{key:?}: nested mappings are not supported")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Note;

    fn input(src: &str) -> NoteInput {
        note_to_input(&Note::parse(src).unwrap()).unwrap()
    }

    #[test]
    fn maps_kind_props_lists_and_links() {
        let i = input(concat!(
            "---\n",
            "topodb-id: 01AAA\n",
            "kind: procedural\n",
            "status: open\n",
            "priority: 2\n",
            "tags: [auth, refactor]\n",
            "relates: \"[[TopoDB]]\"\n",
            "---\n",
            "Use [[redb]] snapshots.\n"
        ));
        assert_eq!(i.id.as_deref(), Some("01AAA"));
        assert_eq!(i.kind.as_deref(), Some("procedural"));
        assert_eq!(i.content, "Use [[redb]] snapshots.");
        let p = i.props.unwrap();
        assert_eq!(p["status"], "open");
        assert_eq!(p["priority"], 2);
        assert_eq!(p["tags"], "auth, refactor"); // list flattens: PropValue has no list type
        assert_eq!(p["relates"], "[[TopoDB]]"); // wikilink prop kept AND linked
        assert!(p.get("topodb-id").is_none());
        assert_eq!(i.entities, vec!["redb", "TopoDB"]); // body first, then frontmatter
    }

    #[test]
    fn related_is_entities_only_never_a_prop() {
        let i = input("---\nrelated:\n  - \"[[A]]\"\n  - \"[[B]]\"\n---\ncontent\n");
        assert_eq!(i.entities, vec!["A", "B"]);
        assert!(i.props.is_none());
    }

    #[test]
    fn entity_stub_flag() {
        let i = input("---\ntopodb-id: 01BBB\nentity: true\n---\n");
        assert!(i.is_entity_stub);
    }

    #[test]
    fn entity_stub_flag_rejects_non_bool() {
        let n = Note::parse("---\nentity: \"yes\"\n---\nx").unwrap();
        let err = note_to_input(&n).unwrap_err();
        assert!(err.contains("entity must be a boolean"), "{err:?}");
    }

    #[test]
    fn rejects_nested_mappings_and_non_string_keys() {
        let n = Note::parse("---\nmeta:\n  deep: 1\n---\nx").unwrap();
        assert!(note_to_input(&n).unwrap_err().contains("nested"));
    }
}
