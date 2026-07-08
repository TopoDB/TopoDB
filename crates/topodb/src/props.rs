use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PropValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
    /// Milliseconds since Unix epoch.
    DateTime(i64),
}

pub type Props = std::collections::BTreeMap<String, PropValue>;
