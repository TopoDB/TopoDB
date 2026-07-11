//! v2 framed values: raw or conditionally lz4-compressed postcard payloads.
use crate::error::TopoError;
use std::borrow::Cow;
pub(crate) const CODEC_RAW: u8 = 0;
pub(crate) const CODEC_LZ4: u8 = 2;
const MAX_DECOMPRESSED_LEN: u32 = 256 * 1024 * 1024;
pub(crate) fn frame_value(raw: Vec<u8>) -> Vec<u8> {
    if raw.len() >= 512 {
        let c = lz4_flex::compress_prepend_size(&raw);
        if c.len() <= raw.len() * 9 / 10 {
            let mut out = Vec::with_capacity(c.len() + 1);
            out.push(CODEC_LZ4);
            out.extend(c);
            return out;
        }
    }
    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(CODEC_RAW);
    out.extend(raw);
    out
}
pub(crate) fn unframe_value(framed: &[u8]) -> Result<Cow<'_, [u8]>, TopoError> {
    match framed.split_first() {
        Some((&CODEC_RAW, r)) => Ok(Cow::Borrowed(r)),
        Some((&CODEC_LZ4, r)) => {
            let b: [u8; 4] = r
                .get(..4)
                .and_then(|x| x.try_into().ok())
                .ok_or_else(|| TopoError::Encoding("truncated lz4 frame".into()))?;
            let n = u32::from_le_bytes(b);
            if n > MAX_DECOMPRESSED_LEN {
                return Err(TopoError::Encoding(format!(
                    "lz4 frame declares {n} bytes, cap is {MAX_DECOMPRESSED_LEN}"
                )));
            }
            lz4_flex::decompress_size_prepended(r)
                .map(Cow::Owned)
                .map_err(|e| TopoError::Encoding(format!("lz4: {e}")))
        }
        Some((c, _)) => Err(TopoError::Encoding(format!(
            "unknown value codec 0x{c:02X}"
        ))),
        None => Err(TopoError::Encoding("empty framed value".into())),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_roundtrips() {
        for n in [0, 511, 512, 4096] {
            let raw = vec![42; n];
            assert_eq!(
                unframe_value(&frame_value(raw.clone())).unwrap().as_ref(),
                raw
            );
        }
    }
    #[test]
    fn rejects_bad() {
        assert!(unframe_value(&[]).is_err());
        assert!(unframe_value(&[1]).is_err());
    }
}
