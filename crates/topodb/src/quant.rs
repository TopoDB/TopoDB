//! SQ8 scalar quantization kernel (format v8). Pure functions, no I/O.
//!
//! Scheme: per-vector max-abs scaling to signed 8-bit codes. The scale
//! CANCELS in cosine, so scoring uses codes alone; `scale` (= maxabs) is
//! stored only so reads can dequantize back to ≈original f32. Invariants
//! pinned by the tests below:
//! - all-zero codes ⟺ the input was exactly zero (max component always
//!   maps to ±127), so the zero-norm doctrine survives quantization;
//! - codes are EXACTLY stable under dequantize→quantize round-trips
//!   (round of integer ± ~2^-22 is the integer); scale is stable to ulps;
//! - every op is IEEE-exact (mul/div/abs/round/sqrt/saturating casts) —
//!   no libm, bit-stable across platforms; `cosine_q`'s accumulation is
//!   pure i64 integer math.
//! - non-finite inputs (unreachable from the live applier, storage.rs:2862;
//!   possible only in pre-v4-era migrated rows) degrade DETERMINISTICALLY:
//!   NaN components code to 0 (saturating cast), a non-finite maxabs
//!   yields the zero vector encoding.

pub(crate) fn quantize(v: &[f32]) -> (f32, Vec<i8>) {
    let mut maxabs = 0.0f32;
    for &x in v {
        let a = x.abs();
        if a > maxabs {
            // NaN comparisons are false, so NaN components never set maxabs.
            maxabs = a;
        }
    }
    if maxabs == 0.0 || !maxabs.is_finite() {
        return (0.0, vec![0i8; v.len()]);
    }
    let s = 127.0f32 / maxabs;
    let codes = v
        .iter()
        .map(|&x| {
            // Saturating float->int cast: NaN -> 0, ±Inf clamps; the extra
            // clamp guards the one-ulp product overshoot past ±127.
            ((x * s).round() as i32).clamp(-127, 127) as i8
        })
        .collect();
    (maxabs, codes)
}

pub(crate) fn dequantize(scale: f32, codes: &[i8]) -> Vec<f32> {
    codes.iter().map(|&c| c as f32 * scale / 127.0).collect()
}

pub(crate) fn is_zero(codes: &[i8]) -> bool {
    codes.iter().all(|&c| c == 0)
}

pub(crate) fn cosine_q(a: &[i8], b: &[i8]) -> Option<f32> {
    let (mut dot, mut na, mut nb) = (0i64, 0i64, 0i64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (i64::from(x), i64::from(y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0 || nb == 0 {
        return None;
    }
    Some(dot as f32 / ((na as f32).sqrt() * (nb as f32).sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// f32 reference cosine, copied from the pre-v8 engine formula
    /// (vector_store.rs::cosine) for error-bound comparison only.
    fn cosine_f32(a: &[f32], b: &[f32]) -> Option<f32> {
        let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
        for (x, y) in a.iter().zip(b) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 {
            return None;
        }
        Some(dot / (na.sqrt() * nb.sqrt()))
    }

    #[test]
    fn zero_vector_codes_are_all_zero_and_scale_zero() {
        let (scale, codes) = quantize(&[0.0; 5]);
        assert_eq!(scale, 0.0);
        assert!(is_zero(&codes));
        assert_eq!(codes.len(), 5);
        assert!(cosine_q(&codes, &codes).is_none());
    }

    #[test]
    fn nonzero_vector_max_component_hits_127() {
        let (scale, codes) = quantize(&[0.5, -2.0, 1.0]);
        assert_eq!(scale, 2.0);
        assert_eq!(codes, vec![32, -127, 64]); // round(0.5*63.5)=32, round(1.0*63.5)=64
        assert!(!is_zero(&codes));
    }

    #[test]
    fn nan_component_codes_to_zero_deterministically() {
        let (scale, codes) = quantize(&[f32::NAN, 1.0]);
        assert_eq!(scale, 1.0);
        assert_eq!(codes, vec![0, 127]);
    }

    #[test]
    fn all_nonfinite_input_degrades_to_zero_encoding() {
        let (scale, codes) = quantize(&[f32::INFINITY, f32::NAN]);
        assert_eq!(scale, 0.0);
        assert!(is_zero(&codes));
    }

    #[test]
    fn cosine_q_identical_codes_is_one() {
        let (_, codes) = quantize(&[0.1, 0.2, -0.3]);
        let c = cosine_q(&codes, &codes).unwrap();
        assert!((c - 1.0).abs() < 1e-6, "self-cosine {c}");
    }

    #[test]
    fn cosine_q_zero_side_is_none() {
        let (_, a) = quantize(&[1.0, 0.0]);
        assert!(cosine_q(&a, &[0, 0]).is_none());
        assert!(cosine_q(&[0, 0], &a).is_none());
    }

    proptest! {
        /// Codes are EXACTLY stable across dequantize -> quantize; the
        /// scale is stable to float ulps (assert 1e-5 relative).
        #[test]
        fn round_trip_codes_are_exactly_stable(
            v in proptest::collection::vec(-1.0f32..1.0, 4..256)
        ) {
            let (s1, c1) = quantize(&v);
            prop_assume!(s1 != 0.0);
            let back = dequantize(s1, &c1);
            let (s2, c2) = quantize(&back);
            prop_assert_eq!(&c1, &c2);
            prop_assert!(((s2 - s1) / s1).abs() < 1e-5, "scale drifted {} -> {}", s1, s2);
        }

        /// Quantized symmetric cosine tracks f32 cosine within a generous
        /// bound (analysis: per-component error <= 0.5/127 of maxabs; for
        /// vectors with norm >= maxabs the cosine deviation is ~0.01).
        #[test]
        fn cosine_q_tracks_f32_cosine(
            a in proptest::collection::vec(-1.0f32..1.0, 8..256),
            b in proptest::collection::vec(-1.0f32..1.0, 8..256),
        ) {
            let n = a.len().min(b.len());
            let (a, b) = (&a[..n], &b[..n]);
            let (fa, fb) = (cosine_f32(a, b), {
                let (_, qa) = quantize(a);
                let (_, qb) = quantize(b);
                cosine_q(&qa, &qb)
            });
            match (fa, fb) {
                (Some(f), Some(q)) => prop_assert!((f - q).abs() < 0.05, "f32 {} vs q {}", f, q),
                // zero-norm agreement: rand vecs in [-1,1] are zero-norm
                // only when literally all-zero, where both sides are None.
                (None, None) => {}
                (f, q) => prop_assert!(false, "zero-norm disagreement: {:?} vs {:?}", f, q),
            }
        }

        /// i64 headroom: a worst-case all-±127 pair at large dim neither
        /// panics (overflow checks are on in tests) nor loses the sign.
        #[test]
        fn i64_accumulators_never_overflow(dim in 1usize..100_000) {
            let a = vec![127i8; dim];
            let b = vec![-127i8; dim];
            let c = cosine_q(&a, &b).unwrap();
            prop_assert!((c + 1.0).abs() < 1e-5);
        }
    }
}
