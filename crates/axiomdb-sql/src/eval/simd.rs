//! SIMD comparison primitives for batch predicate evaluation (Phase 8.2 + 8.7).
//!
//! Uses the `wide` crate for cross-platform SIMD:
//! - **x86_64**: AVX2 (8 × i32) / SSE2 (4 × i32) — auto-detected
//! - **aarch64**: NEON (4 × i32) — always available
//! - **Other**: scalar fallback
//!
//! All functions are **safe Rust** — no `unsafe` blocks.

use super::batch::CmpOp;
use wide::{i32x8, CmpEq, CmpGt, CmpLt};

// ── Batch comparison: i32 ───────────────────────────────────────────────────

/// Compares `values[i]` against `literal` using `op`.
/// Only processes elements where `output[i]` is `true` (AND semantics).
/// Sets `output[i] = false` where the comparison fails.
#[inline]
pub fn batch_cmp_i32(values: &[i32], literal: i32, op: CmpOp, output: &mut [bool]) {
    debug_assert_eq!(values.len(), output.len());

    let n = values.len();
    let lit = i32x8::splat(literal);
    let chunks = n / 8;
    let remainder = n % 8;

    for chunk_idx in 0..chunks {
        let base = chunk_idx * 8;

        // Skip chunk if all elements already failed.
        if !output[base..base + 8].iter().any(|&o| o) {
            continue;
        }

        let arr = [
            values[base],
            values[base + 1],
            values[base + 2],
            values[base + 3],
            values[base + 4],
            values[base + 5],
            values[base + 6],
            values[base + 7],
        ];
        let vals = i32x8::from(arr);

        // wide provides simd_eq, simd_gt, simd_lt.
        // Compose the rest: ne = !eq, ge = !lt, le = !gt.
        // Result: each lane is -1 (0xFFFFFFFF = pass) or 0 (fail).
        let cmp: i32x8 = match op {
            CmpOp::Eq => vals.simd_eq(lit),
            CmpOp::NotEq => {
                let eq = vals.simd_eq(lit);
                // Bitwise NOT: XOR with all-ones (-1 in two's complement).
                eq ^ i32x8::splat(-1)
            }
            CmpOp::Gt => vals.simd_gt(lit),
            CmpOp::Lt => vals.simd_lt(lit),
            CmpOp::GtEq => {
                // a >= b ≡ NOT(a < b)
                let lt = vals.simd_lt(lit);
                lt ^ i32x8::splat(-1)
            }
            CmpOp::LtEq => {
                // a <= b ≡ NOT(a > b)
                let gt = vals.simd_gt(lit);
                gt ^ i32x8::splat(-1)
            }
        };

        let lanes = cmp.to_array();
        for j in 0..8 {
            if lanes[j] == 0 && output[base + j] {
                output[base + j] = false;
            }
        }
    }

    // Remainder with scalar.
    if remainder > 0 {
        let base = chunks * 8;
        scalar_cmp_i32(&values[base..], literal, op, &mut output[base..]);
    }
}

// ── Batch comparison: i64 ───────────────────────────────────────────────────

/// i64 SIMD — scalar is competitive after LLVM auto-vectorization.
#[inline]
pub fn batch_cmp_i64(values: &[i64], literal: i64, op: CmpOp, output: &mut [bool]) {
    debug_assert_eq!(values.len(), output.len());
    for i in 0..values.len() {
        if !output[i] {
            continue;
        }
        if !scalar_cmp(values[i], literal, op) {
            output[i] = false;
        }
    }
}

// ── Batch comparison: bool (u8) ─────────────────────────────────────────────

#[inline]
pub fn batch_cmp_bool(values: &[u8], literal: bool, op: CmpOp, output: &mut [bool]) {
    debug_assert_eq!(values.len(), output.len());
    let lit = u8::from(literal);
    for i in 0..values.len() {
        if !output[i] {
            continue;
        }
        let col = u8::from(values[i] != 0);
        if !scalar_cmp(col, lit, op) {
            output[i] = false;
        }
    }
}

// ── Batch comparison: f64 ───────────────────────────────────────────────────

#[inline]
pub fn batch_cmp_f64(values: &[f64], literal: f64, op: CmpOp, output: &mut [bool]) {
    debug_assert_eq!(values.len(), output.len());
    for i in 0..values.len() {
        if !output[i] {
            continue;
        }
        let pass = match op {
            CmpOp::Eq => values[i] == literal,
            CmpOp::NotEq => values[i] != literal,
            CmpOp::Lt => values[i] < literal,
            CmpOp::LtEq => values[i] <= literal,
            CmpOp::Gt => values[i] > literal,
            CmpOp::GtEq => values[i] >= literal,
        };
        if !pass {
            output[i] = false;
        }
    }
}

// ── Scalar helpers ──────────────────────────────────────────────────────────

#[inline]
fn scalar_cmp<T: Ord>(a: T, b: T, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::NotEq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::LtEq => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::GtEq => a >= b,
    }
}

#[inline]
fn scalar_cmp_i32(values: &[i32], literal: i32, op: CmpOp, output: &mut [bool]) {
    for i in 0..values.len() {
        if !output[i] {
            continue;
        }
        if !scalar_cmp(values[i], literal, op) {
            output[i] = false;
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_cmp_i32_eq() {
        let values = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut output = [true; 16];
        batch_cmp_i32(&values, 5, CmpOp::Eq, &mut output);
        assert_eq!(output[4], true);
        assert_eq!(output.iter().filter(|&&o| o).count(), 1);
    }

    #[test]
    fn test_batch_cmp_i32_gt() {
        let values = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut output = [true; 8];
        batch_cmp_i32(&values, 45, CmpOp::Gt, &mut output);
        assert_eq!(output, [false, false, false, false, true, true, true, true]);
    }

    #[test]
    fn test_batch_cmp_i32_lt_negative() {
        let values = [-10, -5, 0, 5, 10];
        let mut output = [true; 5];
        batch_cmp_i32(&values, 0, CmpOp::Lt, &mut output);
        assert_eq!(output, [true, true, false, false, false]);
    }

    #[test]
    fn test_batch_cmp_i32_and_chain() {
        let a = [1, 2, 3, 4, 5, 6, 7, 8];
        let b = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut output = [true; 8];
        batch_cmp_i32(&a, 3, CmpOp::Gt, &mut output);
        batch_cmp_i32(&b, 65, CmpOp::Lt, &mut output);
        assert_eq!(
            output,
            [false, false, false, true, true, true, false, false]
        );
    }

    #[test]
    fn test_batch_cmp_i32_remainder() {
        let values = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let mut output = [true; 13];
        batch_cmp_i32(&values, 10, CmpOp::GtEq, &mut output);
        let expected = [
            false, false, false, false, false, false, false, false, false, true, true, true, true,
        ];
        assert_eq!(output, expected);
    }

    #[test]
    fn test_batch_cmp_i32_min_max() {
        let values = [i32::MIN, -1, 0, 1, i32::MAX];
        let mut output = [true; 5];
        batch_cmp_i32(&values, 0, CmpOp::GtEq, &mut output);
        assert_eq!(output, [false, false, true, true, true]);
    }

    #[test]
    fn test_batch_cmp_i32_pre_filtered() {
        let values = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut output = [false, true, false, true, false, true, false, true];
        batch_cmp_i32(&values, 4, CmpOp::Gt, &mut output);
        assert_eq!(
            output,
            [false, false, false, false, false, true, false, true]
        );
    }

    #[test]
    fn test_batch_cmp_i32_noteq() {
        let values = [1, 2, 3, 3, 3, 4, 5, 6];
        let mut output = [true; 8];
        batch_cmp_i32(&values, 3, CmpOp::NotEq, &mut output);
        assert_eq!(output, [true, true, false, false, false, true, true, true]);
    }

    #[test]
    fn test_batch_cmp_i32_lteq() {
        let values = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut output = [true; 8];
        batch_cmp_i32(&values, 4, CmpOp::LtEq, &mut output);
        assert_eq!(output, [true, true, true, true, false, false, false, false]);
    }

    #[test]
    fn test_batch_cmp_i64_eq() {
        let values: Vec<i64> = (0..10).collect();
        let mut output = vec![true; 10];
        batch_cmp_i64(&values, 7, CmpOp::Eq, &mut output);
        assert_eq!(output[7], true);
        assert_eq!(output.iter().filter(|&&o| o).count(), 1);
    }

    #[test]
    fn test_batch_cmp_bool_eq() {
        let values = [1u8, 0, 1, 0, 1, 0];
        let mut output = [true; 6];
        batch_cmp_bool(&values, true, CmpOp::Eq, &mut output);
        assert_eq!(output, [true, false, true, false, true, false]);
    }

    #[test]
    fn test_batch_cmp_f64_lt() {
        let values = [1.0, 2.5, 3.0, 4.5, 5.0];
        let mut output = [true; 5];
        batch_cmp_f64(&values, 3.0, CmpOp::Lt, &mut output);
        assert_eq!(output, [true, true, false, false, false]);
    }
}
