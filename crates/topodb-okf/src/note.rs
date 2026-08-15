//! OKF concept page: nested-YAML frontmatter (possibly empty) + exact body.
//!
//! Unlike the obsidian mapper, OKF frontmatter is nested (`generated: {by, at}`,
//! `verified: [{...}]`, `sources: [{...}]`), so the parser keeps the raw
//! `serde_yaml` `Mapping` intact — nested maps and sequences round-trip. The
//! delimiter/body handling mirrors `topodb-obsidian::note` (a shared
//! `topodb-md` core is a recorded follow-up).

use crate::TOPODB_ID_KEY;
use serde_yaml::{Mapping, Value};

/// One OKF page: YAML frontmatter (possibly empty) + exact body text.
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
        // Closing delimiter at the very start of `rest` (empty frontmatter),
        // e.g. "---\n---\nbody\n". Handled inline and returned early.
        if rest.len() >= 3 && &rest[..3] == "---" {
            let after = &rest[3..];
            if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
                let mut body = after;
                if let Some(b) = body
                    .strip_prefix("\r\n")
                    .or_else(|| body.strip_prefix('\n'))
                {
                    body = b;
                }
                return Ok(Note {
                    frontmatter: Mapping::new(),
                    body: body.to_string(),
                });
            }
        }

        let mut close: Option<usize> = None;
        for (i, _) in rest.match_indices("\n---") {
            let after = &rest[i + 4..];
            if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
                close = Some(i);
                break;
            }
        }

        let close_pos = close.ok_or("unterminated frontmatter (no closing ---)")?;
        let yaml = &rest[..close_pos + 1];
        let mut body = &rest[close_pos + 4..];

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
