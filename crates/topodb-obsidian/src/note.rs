use crate::TOPODB_ID_KEY;
use serde_yaml::{Mapping, Value};

/// One vault note: YAML frontmatter (possibly empty) + exact body text.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub frontmatter: Mapping,
    pub body: String,
}

impl Note {
    pub fn parse(text: &str) -> Result<Note, String> {
        let Some(rest) = text
            .strip_prefix("---\n")
            .or_else(|| text.strip_prefix("---\r\n"))
        else {
            return Ok(Note {
                frontmatter: Mapping::new(),
                body: text.to_string(),
            });
        };
        let mut close = None;
        for (i, _) in rest.match_indices("\n---") {
            let after = &rest[i + 4..];
            if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
                close = Some(i);
                break;
            }
        }
        let i = close.ok_or("unterminated frontmatter (no closing ---)")?;
        let yaml = &rest[..i + 1];
        let mut body = &rest[i + 4..];
        if let Some(b) = body
            .strip_prefix("\r\n")
            .or_else(|| body.strip_prefix('\n'))
        {
            body = b;
        }
        let parsed: Value =
            serde_yaml::from_str(yaml).map_err(|e| format!("invalid frontmatter: {e}"))?;
        let frontmatter = match parsed {
            Value::Mapping(m) => m,
            Value::Null => Mapping::new(),
            _ => return Err("frontmatter must be a YAML mapping".into()),
        };
        Ok(Note {
            frontmatter,
            body: body.to_string(),
        })
    }

    pub fn serialize(&self) -> String {
        if self.frontmatter.is_empty() {
            return self.body.clone();
        }
        let yaml = serde_yaml::to_string(&self.frontmatter).expect("mapping serializes");
        format!("---\n{yaml}---\n{}", self.body)
    }

    pub fn id(&self) -> Option<String> {
        self.frontmatter
            .get(Value::String(TOPODB_ID_KEY.into()))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    pub fn set_id(&mut self, id: &str) {
        self.frontmatter.insert(
            Value::String(TOPODB_ID_KEY.into()),
            Value::String(id.into()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let n = Note::parse("---\ntopodb-id: 01ABC\nkind: decision\n---\nBody line.\n").unwrap();
        assert_eq!(n.id().as_deref(), Some("01ABC"));
        assert_eq!(n.body, "Body line.\n");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let n = Note::parse("just text").unwrap();
        assert!(n.frontmatter.is_empty());
        assert_eq!(n.body, "just text");
    }

    #[test]
    fn round_trip_preserves_body_bytes_and_key_order() {
        let src = "---\nzeta: 1\nalpha: two\n---\nBody **exact**\n\n  spacing\n";
        let n = Note::parse(src).unwrap();
        assert_eq!(n.serialize(), src);
    }

    #[test]
    fn set_id_then_serialize_round_trips() {
        let mut n = Note::parse("---\nkind: note\n---\nx\n").unwrap();
        n.set_id("01XYZ");
        let again = Note::parse(&n.serialize()).unwrap();
        assert_eq!(again.id().as_deref(), Some("01XYZ"));
        assert_eq!(again.body, "x\n");
    }

    #[test]
    fn malformed_yaml_is_an_error() {
        assert!(Note::parse("---\n: : :\n---\nx").is_err());
        assert!(Note::parse("---\nnever closed").is_err());
        assert!(Note::parse("---\n- a-list\n---\nx").is_err()); // mapping required
    }
}
