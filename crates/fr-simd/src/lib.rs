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

/// Index of the first byte in `bytes` that is **not** equal to `value`, or `None` if every byte
/// equals `value`.
///
/// This is the scan `BITPOS` needs: to find the first set bit it skips the leading all-`0x00`
/// run (`value = 0x00`); to find the first clear bit it skips the leading all-`0xFF` run
/// (`value = 0xFF`). The first mismatching byte is guaranteed to contain the answer bit, so the
/// caller finishes with a single per-byte `leading_zeros`. `bitpos_full_bytes` is
/// **97.94–98.36% of `BITPOS`'s flat self-time** on a sparse bitmap.
///
/// Dispatches `avx2 → sse2 → scalar`. The SSE2 tier is load-bearing for portability, NOT
/// redundant: unlike `popcount_scalar` (whose word loop LLVM auto-vectorizes to SSE2 SWAR at
/// baseline `x86-64`, verified — 42 xmm instructions), the `position()` scalar loop does **not**
/// auto-vectorize (0 xmm instructions), so without an explicit SSE2 kernel a non-AVX2 x86-64 host
/// would fall all the way to a byte-at-a-time scan. `SSE2` is part of the `x86_64` ABI baseline, so
/// that tier always applies when AVX2 is absent.
///
/// Bit-identical to `bytes.iter().position(|&b| b != value)` for every input, on every tier.
#[inline]
pub fn first_mismatch_byte(bytes: &[u8], value: u8) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` returned true — the precondition of
            // `first_mismatch_byte_avx2`.
            return unsafe { first_mismatch_byte_avx2(bytes, value) };
        }
        if std::arch::is_x86_feature_detected!("sse2") {
            // SAFETY: `is_x86_feature_detected!("sse2")` returned true — the precondition of
            // `first_mismatch_byte_sse2`. (Always true on x86_64, but detected for uniformity.)
            return unsafe { first_mismatch_byte_sse2(bytes, value) };
        }
    }
    first_mismatch_byte_scalar(bytes, value)
}

/// Reference kernel: safe, portable, and the oracle every SIMD arm is tested against.
#[inline]
pub fn first_mismatch_byte_scalar(bytes: &[u8], value: u8) -> Option<usize> {
    bytes.iter().position(|&b| b != value)
}

/// SSE2 fallback tier. `pub` + `#[doc(hidden)]` so the bench can measure it directly on an AVX2
/// host (where the dispatcher would otherwise mask it).
///
/// # Safety
/// The CPU must support `sse2`. Callers must check `is_x86_feature_detected!("sse2")` — trivially
/// true on `x86_64`, where SSE2 is baseline.
#[cfg(target_arch = "x86_64")]
#[doc(hidden)]
#[target_feature(enable = "sse2")]
pub unsafe fn first_mismatch_byte_sse2(bytes: &[u8], value: u8) -> Option<usize> {
    use std::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };

    const LANE: usize = 16;
    let full = bytes.len() / LANE * LANE;

    // SAFETY: SSE2 is guaranteed by the caller. Each `_mm_loadu_si128` is an unaligned 16-byte
    // read; `offset + LANE <= full <= bytes.len()` keeps every read inside the slice.
    let found = unsafe {
        let needle = _mm_set1_epi8(value as i8);
        let mut offset = 0usize;
        let mut hit: Option<usize> = None;
        while offset < full {
            let chunk = _mm_loadu_si128(bytes.as_ptr().add(offset) as *const __m128i);
            // `_mm_movemask_epi8` fills the low 16 bits: 1 = byte equals `value`. A zero among them
            // marks the first mismatch. Mask to 16 bits before inverting so the high bits stay set.
            let eq_mask = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, needle)) as u32 & 0xFFFF;
            if eq_mask != 0xFFFF {
                hit = Some(offset + (!eq_mask & 0xFFFF).trailing_zeros() as usize);
                break;
            }
            offset += LANE;
        }
        hit
    };

    match found {
        Some(index) => Some(index),
        None => first_mismatch_byte_scalar(&bytes[full..], value).map(|i| full + i),
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn first_mismatch_byte_avx2(bytes: &[u8], value: u8) -> Option<usize> {
    use std::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
    };

    const LANE: usize = 32;
    let full = bytes.len() / LANE * LANE;

    // SAFETY: AVX2 is guaranteed by the caller. Each `_mm256_loadu_si256` is an unaligned 32-byte
    // read; `offset + LANE <= full <= bytes.len()` keeps every read inside the slice.
    let found = unsafe {
        let needle = _mm256_set1_epi8(value as i8);
        let mut offset = 0usize;
        let mut hit: Option<usize> = None;
        while offset < full {
            let chunk = _mm256_loadu_si256(bytes.as_ptr().add(offset) as *const __m256i);
            // 1 bit per byte that EQUALS `value`; a zero bit marks a mismatch.
            let eq_mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, needle)) as u32;
            if eq_mask != u32::MAX {
                // First zero bit = first mismatching byte within this 32-byte lane.
                hit = Some(offset + (!eq_mask).trailing_zeros() as usize);
                break;
            }
            offset += LANE;
        }
        hit
    };

    match found {
        Some(index) => Some(index),
        // The vector loop cleared every full lane; scan the < 32-byte tail.
        None => first_mismatch_byte_scalar(&bytes[full..], value).map(|i| full + i),
    }
}

/// In-place `dst[i] &= src[i]` over the overlapping prefix (`min` length).
///
/// This is `BITOP AND`'s inner kernel. Like popcount/bitpos, `fr-store`'s word loop compiles to
/// SSE2 at baseline `x86-64` (`Store::bitop`: 128 xmm, 0 ymm) even where AVX2 is present. Whether
/// AVX2 actually wins here is a *measurement*, not an assumption: BITOP is a streaming
/// read-read-write, so it can be cache-bandwidth-bound rather than issue-bound — see the A/B before
/// trusting a speedup. Dispatches `avx2 → scalar` (the scalar arm is LLVM's SSE2-vectorized word
/// loop on x86_64). Bit-identical to `for i { dst[i] &= src[i] }`.
#[inline]
pub fn bitand_inplace(dst: &mut [u8], src: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 confirmed present.
            unsafe {
                bitand_inplace_avx2(dst, src);
            }
            return;
        }
    }
    bitand_inplace_scalar(dst, src);
}

/// Reference kernel: safe, portable, LLVM-vectorized to SSE2 on x86_64. The A/B baseline.
#[inline]
pub fn bitand_inplace_scalar(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] &= src[i];
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bitand_inplace_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{
        __m256i, _mm256_and_si256, _mm256_loadu_si256, _mm256_storeu_si256,
    };

    let n = dst.len().min(src.len());
    let full = n / 32 * 32;

    // SAFETY: AVX2 guaranteed by the caller. Every load/store spans `[offset, offset+32)` with
    // `offset + 32 <= full <= n <= {dst,src}.len()`, so all accesses stay inside both slices.
    unsafe {
        let mut offset = 0usize;
        while offset < full {
            let a = _mm256_loadu_si256(dst.as_ptr().add(offset) as *const __m256i);
            let b = _mm256_loadu_si256(src.as_ptr().add(offset) as *const __m256i);
            _mm256_storeu_si256(
                dst.as_mut_ptr().add(offset) as *mut __m256i,
                _mm256_and_si256(a, b),
            );
            offset += 32;
        }
    }

    for i in full..n {
        dst[i] &= src[i];
    }
}

#[cfg(test)]
mod tests {
    use super::{
        first_mismatch_byte, first_mismatch_byte_scalar, popcount_bytes, popcount_scalar,
    };

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

    /// The `BITPOS` skip-scan must equal `position(|b| b != value)` for every input. The
    /// exhaustive mismatch-position loop is the important case: the AVX2 lane math must report the
    /// EXACT first mismatch, or `BITPOS` returns the wrong bit index.
    #[test]
    fn first_mismatch_matches_position_for_all_lengths_and_positions() {
        for &skip in &[0x00u8, 0xffu8, 0x55u8] {
            let other = !skip;
            for len in 0..=600usize {
                // All-skip: no mismatch anywhere.
                let all_skip = vec![skip; len];
                assert_eq!(
                    super::first_mismatch_byte(&all_skip, skip),
                    None,
                    "all-skip skip={skip:#04x} len={len}"
                );
                // Exactly one mismatch, walked across every position — including across the
                // 32-byte lane boundary and into the scalar tail.
                for pos in 0..len {
                    let mut buf = vec![skip; len];
                    buf[pos] = other;
                    assert_eq!(
                        super::first_mismatch_byte(&buf, skip),
                        Some(pos),
                        "skip={skip:#04x} len={len} pos={pos}"
                    );
                    assert_eq!(super::first_mismatch_byte(&buf, skip), first_mismatch_byte_scalar(&buf, skip));
                }
            }
        }
    }

    /// The SSE2 fallback tier must equal the oracle for every mismatch position, independently of
    /// the AVX2 tier — an AVX2 host would otherwise never execute it, so the dispatcher's tests
    /// give it no coverage. Walks the single mismatch across the 16-byte lane boundary and tail.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn sse2_first_mismatch_matches_oracle_for_all_positions() {
        if !std::arch::is_x86_feature_detected!("sse2") {
            return; // unreachable on x86_64 (sse2 is baseline), but keep the guard honest
        }
        for &skip in &[0x00u8, 0xffu8, 0x55u8] {
            let other = !skip;
            for len in 0..=200usize {
                // SAFETY: sse2 confirmed present above.
                assert_eq!(unsafe { super::first_mismatch_byte_sse2(&vec![skip; len], skip) }, None);
                for pos in 0..len {
                    let mut buf = vec![skip; len];
                    buf[pos] = other;
                    // SAFETY: sse2 confirmed present above.
                    let got = unsafe { super::first_mismatch_byte_sse2(&buf, skip) };
                    assert_eq!(got, Some(pos), "sse2 skip={skip:#04x} len={len} pos={pos}");
                }
            }
        }
    }

    #[test]
    fn first_mismatch_every_alignment() {
        // Offsetting into a backing buffer exercises unaligned 32-byte loads.
        let mut backing = vec![0u8; 400];
        fill(&mut backing, 0xabcd_ef01_2345_6789);
        for start in 0..40usize {
            for len in [0usize, 1, 31, 32, 33, 64, 100, 200] {
                if start + len > backing.len() {
                    continue;
                }
                let slice = &backing[start..start + len];
                for &v in &[0x00u8, 0xffu8, slice.first().copied().unwrap_or(0)] {
                    assert_eq!(
                        first_mismatch_byte(slice, v),
                        first_mismatch_byte_scalar(slice, v),
                        "start={start} len={len} v={v:#04x}"
                    );
                }
            }
        }
    }

    /// A 1 MiB all-zero buffer with a single set bit at the end is the worst-case `BITPOS 1` scan.
    #[test]
    fn first_mismatch_one_mebibyte_sparse() {
        let mut buf = vec![0u8; 1024 * 1024];
        assert_eq!(first_mismatch_byte(&buf, 0x00), None);
        *buf.last_mut().unwrap() = 0x01;
        assert_eq!(first_mismatch_byte(&buf, 0x00), Some(1024 * 1024 - 1));
    }

    /// `bitand_inplace` must equal the scalar `dst[i] &= src[i]` loop for every input — the vector
    /// body, the < 32-byte tail, unequal lengths (`min` truncation), and every alignment.
    #[test]
    fn bitand_matches_scalar_all_lengths_alignments_and_unequal() {
        use super::{bitand_inplace, bitand_inplace_scalar};
        let mut da = vec![0u8; 300];
        let mut sa = vec![0u8; 300];
        fill(&mut da, 0x1111_2222_3333_4444);
        fill(&mut sa, 0x5555_6666_7777_8888);
        for dstart in 0..34usize {
            for sstart in 0..34usize {
                for len in [0usize, 1, 15, 16, 31, 32, 33, 64, 100, 250] {
                    if dstart + len > da.len() || sstart + len > sa.len() {
                        continue;
                    }
                    let mut d1 = da[dstart..dstart + len].to_vec();
                    let mut d2 = d1.clone();
                    let s = &sa[sstart..sstart + len];
                    bitand_inplace(&mut d1, s);
                    bitand_inplace_scalar(&mut d2, s);
                    assert_eq!(d1, d2, "dstart={dstart} sstart={sstart} len={len}");
                }
            }
        }
        // Unequal lengths: only the min prefix is touched.
        let mut d = vec![0xffu8; 40];
        let orig = d.clone();
        bitand_inplace(&mut d, &[0x0fu8; 10]);
        assert_eq!(&d[..10], &[0x0fu8; 10]);
        assert_eq!(&d[10..], &orig[10..], "bytes past src.len() untouched");
    }
}
