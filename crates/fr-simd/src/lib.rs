//! Narrow, audited SIMD kernels behind safe interfaces.
//!
//! This is the **one** place in the workspace where `unsafe` is permitted, and only under the
//! terms AGENTS.md sets: *"If narrow unsafe usage is unavoidable, isolate it behind audited
//! interfaces and tests."* Every other crate keeps `#![forbid(unsafe_code)]`, including
//! `fr-store`, which calls into here.
//!
//! ## Why unsafe is unavoidable here
//!
//! `fr-store`'s `BITCOUNT` kernel is **97.94% of that command's flat self-time**, and the build
//! emits an **SSE2 software popcount** for it: the release profile sets no `target-cpu`, so codegen
//! targets baseline `x86-64`, which excludes `POPCNT` (SSE4.2) and AVX2 — even on hosts that have
//! both. Measured on `thinkstation1`, one binary, arms verified to differ in machine code:
//!
//! | kernel | GiB/s |
//! |---|---:|
//! | SSE2 SWAR (what we emit today) | 17.19 |
//! | scalar hardware `POPCNT` | 30.75 |
//! | **AVX2 nibble-LUT** | **53.91** |
//!
//! Reaching AVX2 at *runtime* requires `#[target_feature(enable = "avx2")]`, and calling such a
//! function from a context that lacks the feature requires `unsafe`. Portable `core::simd` does
//! **not** help: its codegen is bounded by the enabled target features, so `Simd<u8, 32>` lowers to
//! two SSE2 vectors on a baseline build. The only alternative is `target-cpu=x86-64-v3`, which
//! raises the binary's minimum CPU to 2013-era Haswell. This crate takes the runtime-dispatch route
//! instead, so the shipped binary still runs anywhere `x86-64` runs.
//!
//! ## Safety argument
//!
//! Each `#[target_feature]` function is `unsafe` solely because the caller must guarantee the CPU
//! supports the feature. [`popcount_bytes`] is the only caller, and it establishes that guarantee
//! immediately before each call via `is_x86_feature_detected!`. The bodies perform no pointer
//! arithmetic beyond `_mm*_loadu_si*` reads that are bounds-checked by the loop condition against
//! `bytes.len()`, and every tail byte is handled by the safe scalar path. `popcount_scalar` is the
//! reference: the unit tests assert every dispatched arm agrees with it **bit-for-bit** across all
//! lengths `0..=1024`, all alignments, and adversarial bit patterns.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

/// Population count over a byte slice.
///
/// Bit-identical to `bytes.iter().map(|b| b.count_ones()).sum()` for every input, because
/// popcount is order-independent. Dispatches once per call to the widest kernel this CPU
/// supports; the scalar fallback is always correct and is what non-`x86_64` targets use.
#[inline]
pub fn popcount_bytes(bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        // AVX2 first: measured 1.75x faster than scalar POPCNT and 3.14x faster than the
        // SSE2 SWAR the baseline build emits.
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` returned true on this CPU, which is
            // exactly the precondition of `popcount_avx2`.
            return unsafe { popcount_avx2(bytes) };
        }
        if std::arch::is_x86_feature_detected!("popcnt") {
            // SAFETY: `is_x86_feature_detected!("popcnt")` returned true on this CPU, which is
            // exactly the precondition of `popcount_popcnt`.
            return unsafe { popcount_popcnt(bytes) };
        }
    }
    popcount_scalar(bytes)
}

/// Reference kernel. Safe, portable, and the oracle every other arm is tested against.
///
/// Eight bytes per iteration as one 64-bit `count_ones`; on a baseline `x86-64` build LLVM
/// auto-vectorizes this to the SSE2 `psrlw/pand/paddb/psadbw/paddq` sequence.
#[inline]
pub fn popcount_scalar(bytes: &[u8]) -> usize {
    let (chunks, remainder) = bytes.as_chunks::<8>();
    let mut count: usize = 0;
    for chunk in chunks {
        count += u64::from_ne_bytes(*chunk).count_ones() as usize;
    }
    for &byte in remainder {
        count += byte.count_ones() as usize;
    }
    count
}

/// # Safety
/// The CPU must support `popcnt`. Callers must check `is_x86_feature_detected!("popcnt")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
unsafe fn popcount_popcnt(bytes: &[u8]) -> usize {
    // Identical source to `popcount_scalar`; the target feature is what changes the lowering,
    // turning the SWAR sequence into a single `popcnt` per 64-bit word.
    popcount_scalar(bytes)
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn popcount_avx2(bytes: &[u8]) -> usize {
    use std::arch::x86_64::{
        __m256i, _mm256_add_epi8, _mm256_add_epi64, _mm256_and_si256, _mm256_loadu_si256,
        _mm256_sad_epu8, _mm256_setr_epi8, _mm256_setzero_si256, _mm256_set1_epi8,
        _mm256_shuffle_epi8, _mm256_srli_epi16, _mm256_storeu_si256,
    };

    const LANE: usize = 32;
    let full = bytes.len() / LANE * LANE;

    // SAFETY: every intrinsic below is an AVX2 instruction, and the caller has guaranteed AVX2 is
    // available. `_mm256_loadu_si256` is an unaligned 32-byte read; `offset + LANE <= full <=
    // bytes.len()` bounds every read inside the slice. The `_mm256_storeu_si256` writes into a
    // local `[u64; 4]`, whose 32-byte size matches the register width exactly.
    let total = unsafe {
        // Nibble popcount lookup table, duplicated across both 128-bit halves because
        // `_mm256_shuffle_epi8` shuffles within lanes.
        let lut = _mm256_setr_epi8(
            0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, //
            0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
        );
        let low_nibble = _mm256_set1_epi8(0x0f);
        let zero = _mm256_setzero_si256();
        let mut acc = _mm256_setzero_si256();

        let mut offset = 0usize;
        while offset < full {
            let v = _mm256_loadu_si256(bytes.as_ptr().add(offset) as *const __m256i);
            let lo = _mm256_and_si256(v, low_nibble);
            let hi = _mm256_and_si256(_mm256_srli_epi16(v, 4), low_nibble);
            let counts = _mm256_add_epi8(
                _mm256_shuffle_epi8(lut, lo),
                _mm256_shuffle_epi8(lut, hi),
            );
            // `sad_epu8` horizontally sums each 8-byte group into a u64 lane; the per-byte counts
            // are <= 8, so 8 of them cannot overflow a byte, and the u64 accumulator cannot
            // overflow for any slice that fits in memory.
            acc = _mm256_add_epi64(acc, _mm256_sad_epu8(counts, zero));
            offset += LANE;
        }

        let mut lanes = [0u64; 4];
        _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
        lanes[0] + lanes[1] + lanes[2] + lanes[3]
    };

    total as usize + popcount_scalar(&bytes[full..])
}

#[cfg(test)]
mod tests {
    use super::{popcount_bytes, popcount_scalar};

    /// The oracle: the definition of popcount, straight from the standard library.
    fn oracle(bytes: &[u8]) -> usize {
        bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Deterministic pseudo-random fill; no dependency on `rand`.
    fn fill(buf: &mut [u8], mut seed: u64) {
        for byte in buf.iter_mut() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *byte = (seed >> 33) as u8;
        }
    }

    #[test]
    fn every_dispatched_arm_matches_the_oracle_for_all_lengths_to_1024() {
        // Covers the vector body, every possible tail remainder (0..32), and the sub-lane sizes
        // where the AVX2 loop never executes at all.
        let mut buf = vec![0u8; 1024];
        for seed in [1u64, 0xdead_beef, u64::MAX] {
            fill(&mut buf, seed);
            for len in 0..=1024usize {
                let slice = &buf[..len];
                let want = oracle(slice);
                assert_eq!(popcount_bytes(slice), want, "dispatched, len={len}, seed={seed}");
                assert_eq!(popcount_scalar(slice), want, "scalar, len={len}, seed={seed}");
            }
        }
    }

    #[test]
    fn adversarial_bit_patterns_and_every_alignment() {
        // Offsetting into the buffer exercises unaligned 32-byte loads.
        let patterns: [u8; 6] = [0x00, 0xff, 0x55, 0xaa, 0x0f, 0xf0];
        for pattern in patterns {
            let buf = vec![pattern; 300];
            for start in 0..32usize {
                for len in [0usize, 1, 7, 8, 31, 32, 33, 63, 64, 65, 128, 200] {
                    if start + len > buf.len() {
                        continue;
                    }
                    let slice = &buf[start..start + len];
                    assert_eq!(
                        popcount_bytes(slice),
                        oracle(slice),
                        "pattern={pattern:#04x} start={start} len={len}"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_and_single_byte() {
        assert_eq!(popcount_bytes(&[]), 0);
        for b in 0..=255u8 {
            assert_eq!(popcount_bytes(&[b]), b.count_ones() as usize);
        }
    }

    /// A 1 MiB buffer is the shape `BITCOUNT` actually runs on.
    #[test]
    fn one_mebibyte_matches_the_oracle() {
        let mut buf = vec![0u8; 1024 * 1024];
        fill(&mut buf, 0x1234_5678_9abc_def0);
        assert_eq!(popcount_bytes(&buf), oracle(&buf));
    }
}
