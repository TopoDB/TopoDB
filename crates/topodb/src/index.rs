//! Declarative index configuration (`IndexSpec`) plus the hashable subset of
//! `PropValue` (`IndexValue`) used as the equality-index key. `IndexSpec` is
//! supplied at `Db::open_with` time and carried in `Storage`/`Snapshot`;
//! `graph.rs` is the only place that reads it to maintain `Snapshot::prop_index`.

use crate::error::TopoError;
use crate::props::PropValue;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::collections::HashSet;

/// A single declared `(label, prop)` pair to index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropIndex {
    pub label: SmolStr,
    pub prop: String,
}

/// Declares which `(label, prop)` pairs get indexed, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSpec {
    /// Hash-indexed for equality lookup: `Db::nodes_by_prop`.
    pub equality: Vec<PropIndex>,
    /// Tokenized + BM25-indexed: `search_text` (wired in Task 6).
    pub text: Vec<PropIndex>,
}

impl IndexSpec {
    /// Rejects duplicate `(label, prop)` declarations within `equality` or
    /// within `text` (checked independently — declaring the same key in both
    /// lists is a valid "index this both ways" request, not a duplicate).
    ///
    /// Deliberately does *not* try to reject an equality declaration whose
    /// values will turn out to be `Float`: `IndexSpec` carries no value-type
    /// info, so that can't be known at open time without scanning existing
    /// data. Instead, Float is rejected per-VALUE: `IndexValue::of` returns
    /// `None` for it (so such values are simply never entered into the
    /// index), and `Db::nodes_by_prop` rejects a Float query value outright.
    pub(crate) fn validate(&self) -> Result<(), TopoError> {
        validate_unique(&self.equality)?;
        validate_unique(&self.text)?;
        Ok(())
    }
}

fn validate_unique(entries: &[PropIndex]) -> Result<(), TopoError> {
    let mut seen: HashSet<(&SmolStr, &str)> = HashSet::new();
    for p in entries {
        if !seen.insert((&p.label, p.prop.as_str())) {
            return Err(TopoError::Rejected(format!(
                "duplicate index declaration for ({}, {})",
                p.label, p.prop
            )));
        }
    }
    Ok(())
}

/// Hashable subset of `PropValue`, used as the equality-index key. `Float` is
/// deliberately absent — floats are not equality-indexable (see
/// `nodes_by_float_range` for the intended access pattern instead).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum IndexValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Bytes(Vec<u8>),
    DateTime(i64),
}

impl IndexValue {
    /// `None` for `PropValue::Float` — every other variant converts 1:1.
    pub(crate) fn of(v: &PropValue) -> Option<IndexValue> {
        match v {
            PropValue::Str(s) => Some(IndexValue::Str(s.clone())),
            PropValue::Int(i) => Some(IndexValue::Int(*i)),
            PropValue::Bool(b) => Some(IndexValue::Bool(*b)),
            PropValue::Bytes(b) => Some(IndexValue::Bytes(b.clone())),
            PropValue::DateTime(dt) => Some(IndexValue::DateTime(*dt)),
            PropValue::Float(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_duplicates_within_a_list_but_not_across_lists() {
        let dup_within = IndexSpec {
            equality: vec![
                PropIndex {
                    label: "M".into(),
                    prop: "x".into(),
                },
                PropIndex {
                    label: "M".into(),
                    prop: "x".into(),
                },
            ],
            text: vec![],
        };
        assert!(matches!(dup_within.validate(), Err(TopoError::Rejected(_))));

        let same_key_both_lists = IndexSpec {
            equality: vec![PropIndex {
                label: "M".into(),
                prop: "x".into(),
            }],
            text: vec![PropIndex {
                label: "M".into(),
                prop: "x".into(),
            }],
        };
        assert!(same_key_both_lists.validate().is_ok());
    }

    #[test]
    fn index_value_of_is_none_for_float_and_some_for_everything_else() {
        assert_eq!(IndexValue::of(&PropValue::Float(1.0)), None);
        assert_eq!(
            IndexValue::of(&PropValue::Str("a".into())),
            Some(IndexValue::Str("a".into()))
        );
        assert_eq!(IndexValue::of(&PropValue::Int(1)), Some(IndexValue::Int(1)));
        assert_eq!(
            IndexValue::of(&PropValue::Bool(true)),
            Some(IndexValue::Bool(true))
        );
        assert_eq!(
            IndexValue::of(&PropValue::Bytes(vec![1, 2])),
            Some(IndexValue::Bytes(vec![1, 2]))
        );
        assert_eq!(
            IndexValue::of(&PropValue::DateTime(5)),
            Some(IndexValue::DateTime(5))
        );
    }
}
