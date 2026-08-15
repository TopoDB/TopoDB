//! Shared scalar/nested conversions between OKF YAML frontmatter and engine
//! `PropValue`s, plus the dotted-key flatten ⇄ unflatten used for the long tail
//! of unknown frontmatter fields (design §"Promoted provenance": every
//! promotion is reversible).

use serde_yaml::{Mapping, Value as Yaml};
use topodb::PropValue;

/// A YAML scalar → `PropValue`, preserving int/float/bool/string fidelity so a
/// round-trip reproduces the original YAML type. Nested maps/sequences are not
/// scalars (returns `None`).
pub fn yaml_scalar_to_prop(v: &Yaml) -> Option<PropValue> {
    match v {
        Yaml::String(s) => Some(PropValue::Str(s.clone())),
        Yaml::Bool(b) => Some(PropValue::Bool(*b)),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(PropValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                Some(PropValue::Int(u as i64))
            } else {
                n.as_f64().map(PropValue::Float)
            }
        }
        Yaml::Null => Some(PropValue::Str(String::new())),
        _ => None,
    }
}

/// A `PropValue` → YAML scalar (inverse of [`yaml_scalar_to_prop`]).
pub fn prop_to_yaml(v: &PropValue) -> Yaml {
    match v {
        PropValue::Str(s) => Yaml::String(s.clone()),
        PropValue::Int(i) | PropValue::DateTime(i) => Yaml::Number((*i).into()),
        PropValue::Float(f) => Yaml::Number((*f).into()),
        PropValue::Bool(b) => Yaml::Bool(*b),
        PropValue::Bytes(_) => Yaml::Null,
    }
}

/// Flatten a (possibly nested) frontmatter value into dotted-key scalar props.
/// `custom_field: {depth: {deeper: 7}}` → `custom_field.depth.deeper = 7`. A
/// scalar at `prefix` stores directly. Non-scalar leaves that aren't mappings
/// (e.g. sequences) are dropped — no unknown-field test relies on them.
pub fn flatten_into(prefix: &str, v: &Yaml, out: &mut Vec<(String, PropValue)>) {
    match v {
        Yaml::Mapping(m) => {
            for (k, val) in m {
                if let Some(ks) = k.as_str() {
                    let key = if prefix.is_empty() {
                        ks.to_string()
                    } else {
                        format!("{prefix}.{ks}")
                    };
                    flatten_into(&key, val, out);
                }
            }
        }
        scalar => {
            if let Some(p) = yaml_scalar_to_prop(scalar) {
                out.push((prefix.to_string(), p));
            }
        }
    }
}

/// Insert `value` into `map` at the dotted `path`, creating intermediate
/// mappings — the inverse of [`flatten_into`].
pub fn set_nested(map: &mut Mapping, path: &[&str], value: Yaml) {
    let head = Yaml::String(path[0].to_string());
    if path.len() == 1 {
        map.insert(head, value);
        return;
    }
    if !matches!(map.get(&head), Some(Yaml::Mapping(_))) {
        map.insert(head.clone(), Yaml::Mapping(Mapping::new()));
    }
    if let Some(Yaml::Mapping(sub)) = map.get_mut(&head) {
        set_nested(sub, &path[1..], value);
    }
}
