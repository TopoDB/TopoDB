//! v7 -> v8: quantize every `vectors` row from framed postcard `Vec<f32>`
//! to framed postcard `(f32, Vec<i8>)` = (scale, codes), in place. Runs
//! inside the open transaction's version dispatch for files stamped 4..=7
//! (VECTORS has existed since v4). Files stamped 2/3 must NOT run this
//! pass: their `migrate_v4` chain writes rows through the v8 `put_vector`,
//! which already quantizes — and re-decoding a `(f32, Vec<i8>)` row as
//! `Vec<f32>` is not guaranteed to error (postcard has no self-description),
//! so a double run could silently corrupt. Keys+values are collected before
//! rewriting (redb tables cannot be mutated mid-iteration); at the 1M×384
//! tier that is ~450 MB transient — acceptable for a one-time migration on
//! the machines this ships to, noted in FORMAT.md.
use crate::codec::{frame_value, unframe_value};
use crate::error::{storage_err, TopoError};
use crate::quant;
use crate::vector_store::VECTORS;
use redb::{ReadableTable, WriteTransaction};

pub(crate) fn quantize_vectors(tx: &WriteTransaction) -> Result<(), TopoError> {
    let mut table = tx.open_table(VECTORS).map_err(storage_err)?;
    let mut rewrites: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        for entry in table.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let raw = unframe_value(v.value())?;
            let vec: Vec<f32> = postcard::from_bytes(&raw)
                .map_err(|e| TopoError::Encoding(format!("v8 vectors migration: {e}")))?;
            let (scale, codes) = quant::quantize(&vec);
            let out = postcard::to_allocvec(&(scale, codes))
                .map_err(|e| TopoError::Encoding(format!("v8 vectors migration: {e}")))?;
            rewrites.push((k.value().to_vec(), frame_value(out)));
        }
    }
    for (k, v) in rewrites {
        table
            .insert(k.as_slice(), v.as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizes_v7_f32_rows_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(crate::vector_store::VECTORS).unwrap();
            let raw = postcard::to_allocvec(&vec![0.5f32, -2.0, 1.0]).unwrap();
            vectors
                .insert(
                    crate::vector_store::vector_key(1, 1, 7).as_slice(),
                    crate::codec::frame_value(raw).as_slice(),
                )
                .unwrap();
            let mut refs = tx.open_table(crate::vector_store::EMBEDDING_REF).unwrap();
            let ref_raw = postcard::to_allocvec(&(1u32, 1u32)).unwrap();
            refs.insert(7u64.to_be_bytes().as_slice(), ref_raw.as_slice())
                .unwrap();
        }
        quantize_vectors(&tx).unwrap();
        {
            let vectors = tx.open_table(crate::vector_store::VECTORS).unwrap();
            let refs = tx.open_table(crate::vector_store::EMBEDDING_REF).unwrap();
            let (_, _, scale, codes) = crate::vector_store::read_qvec_by_slot(&vectors, &refs, 7)
                .unwrap()
                .unwrap();
            assert_eq!(scale, 2.0);
            assert_eq!(codes, vec![32i8, -127, 64]);
        }
        tx.commit().unwrap();
    }
}
