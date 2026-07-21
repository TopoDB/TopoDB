use topodb::TopoError;

/// Exhaustive by construction — no wildcard arm.
fn code(e: &TopoError) -> &'static str {
    match e {
        TopoError::Storage(_) => "STORAGE",
        TopoError::Encoding(_) => "ENCODING",
        TopoError::Rejected(_) => "REJECTED",
        TopoError::Compacted { .. } => "COMPACTED",
        TopoError::Closed => "CLOSED",
        TopoError::UnsupportedFormat { .. } => "UNSUPPORTED_FORMAT",
    }
}

pub fn to_napi(e: TopoError) -> napi::Error {
    napi::Error::from_reason(format!("[{}] {}", code(&e), e))
}

pub fn rejected(msg: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(format!("[REJECTED] {msg}"))
}

pub fn closed() -> napi::Error {
    napi::Error::from_reason("[CLOSED] database closed".to_string())
}
