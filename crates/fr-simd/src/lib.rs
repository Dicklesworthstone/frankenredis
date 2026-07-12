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

/// Length of the common prefix of `a` and `b`: the smallest `i` where `a[i] != b[i]`, or
/// `min(a.len(), b.len())` if one is a prefix of the other.
///
/// This is `lzf_compress`'s **match-extension** inner loop (fr-persist), which is up to **11.4% of
/// flat self-time on a multi-node quicklist DUMP**. It is the two-array sibling of
/// [`first_mismatch_byte`]: `_mm256_cmpeq_epi8(a_chunk, b_chunk)` + movemask, first zero bit = first
/// differing byte. `fr-persist`'s current word loop stays scalar/SSE2 at baseline. Dispatches
/// `avx2 → sse2 → scalar`; the SSE2 tier matters because a plain byte/word compare loop does not
/// reliably auto-vectorize (same asymmetry proven for `first_mismatch`).
///
/// Bit-identical to the scalar loop for every input, so LZF output stays byte-exact.
/// Below this common length the scalar word loop is faster than a SIMD compare — the vector
/// setup costs more than it saves on a short match. Measured on `hz2` (fit null ~1.0): AVX2
/// regresses at 16 B (0.53x) and 32 B (0.89x), is *indistinguishable* at 64 B (1.16x, inside the
/// null spread), and is a decidable WIN only from **128 B (1.81x)** up (2.13x @256, 2.88x @512,
/// each above its null p95). Gating at 128 makes the kernel **Pareto-safe**: below it the caller
/// runs the byte-identical scalar loop, so this can never regress whatever the length distribution
/// (LZF matches are often short), and it only takes the SIMD path where the win is decidable.
const SIMD_MIN_LEN: usize = 128;

#[inline]
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if n >= SIMD_MIN_LEN {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: avx2 confirmed present; n >= SIMD_MIN_LEN <= min(a,b).len().
                return unsafe { common_prefix_len_avx2(a, b, n) };
            }
            if std::arch::is_x86_feature_detected!("sse2") {
                // SAFETY: sse2 confirmed present (baseline on x86_64).
                return unsafe { common_prefix_len_sse2(a, b, n) };
            }
        }
    }
    common_prefix_len_scalar(a, b, n)
}

/// Reference kernel: safe, portable, the oracle the SIMD arms are tested against. 8 bytes/iter with
/// a little-endian `trailing_zeros` first-difference, then a byte tail — verbatim of the loop
/// `fr-persist` shipped.
#[inline]
pub fn common_prefix_len_scalar(a: &[u8], b: &[u8], n: usize) -> usize {
    let mut i = 0;
    while i + 8 <= n {
        let d = u64::from_le_bytes(a[i..i + 8].try_into().unwrap())
            ^ u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if d != 0 {
            return i + (d.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n {
        if a[i] != b[i] {
            return i;
        }
        i += 1;
    }
    n
}

/// # Safety
/// The CPU must support `avx2`, and `n <= min(a.len(), b.len())`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn common_prefix_len_avx2(a: &[u8], b: &[u8], n: usize) -> usize {
    use std::arch::x86_64::{
        __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
    };
    let full = n / 32 * 32;
    // SAFETY: AVX2 guaranteed; every load spans `[off, off+32) ⊆ [0, full) ⊆ [0, n) ⊆` both slices.
    unsafe {
        let mut off = 0usize;
        while off < full {
            let av = _mm256_loadu_si256(a.as_ptr().add(off) as *const __m256i);
            let bv = _mm256_loadu_si256(b.as_ptr().add(off) as *const __m256i);
            // 1 bit per byte that is EQUAL; a zero marks the first difference in this lane.
            let eq = _mm256_movemask_epi8(_mm256_cmpeq_epi8(av, bv)) as u32;
            if eq != u32::MAX {
                return off + (!eq).trailing_zeros() as usize;
            }
            off += 32;
        }
    }
    common_prefix_len_scalar(&a[full..n], &b[full..n], n - full) + full
}

/// # Safety
/// The CPU must support `sse2` (baseline on x86_64), and `n <= min(a.len(), b.len())`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn common_prefix_len_sse2(a: &[u8], b: &[u8], n: usize) -> usize {
    use std::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
    let full = n / 16 * 16;
    // SAFETY: SSE2 guaranteed; every load spans `[off, off+16) ⊆ [0, full) ⊆ [0, n) ⊆` both slices.
    unsafe {
        let mut off = 0usize;
        while off < full {
            let av = _mm_loadu_si128(a.as_ptr().add(off) as *const __m128i);
            let bv = _mm_loadu_si128(b.as_ptr().add(off) as *const __m128i);
            let eq = _mm_movemask_epi8(_mm_cmpeq_epi8(av, bv)) as u32 & 0xFFFF;
            if eq != 0xFFFF {
                return off + (!eq & 0xFFFF).trailing_zeros() as usize;
            }
            off += 16;
        }
    }
    common_prefix_len_scalar(&a[full..n], &b[full..n], n - full) + full
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

/// In-place `dst[i] |= src[i]` over the overlapping prefix (`min` length). `BITOP OR`'s
/// accumulate kernel. Same streaming read-read-write shape as [`bitand_inplace`]; the AVX2
/// A/B that settled AND applies verbatim (only the 1-cycle lane op differs). Dispatches
/// `avx2 → scalar`. Bit-identical to `for i { dst[i] |= src[i] }`.
#[inline]
pub fn bitor_inplace(dst: &mut [u8], src: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 confirmed present.
            unsafe {
                bitor_inplace_avx2(dst, src);
            }
            return;
        }
    }
    bitor_inplace_scalar(dst, src);
}

/// Reference kernel: safe, portable, LLVM-vectorized to SSE2 on x86_64. The A/B baseline.
#[inline]
pub fn bitor_inplace_scalar(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] |= src[i];
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bitor_inplace_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_or_si256, _mm256_storeu_si256};
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
                _mm256_or_si256(a, b),
            );
            offset += 32;
        }
    }
    for i in full..n {
        dst[i] |= src[i];
    }
}

/// In-place `dst[i] ^= src[i]` over the overlapping prefix (`min` length). `BITOP XOR`'s
/// accumulate kernel. Same shape/dispatch as [`bitand_inplace`]. Bit-identical to
/// `for i { dst[i] ^= src[i] }`.
#[inline]
pub fn bitxor_inplace(dst: &mut [u8], src: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 confirmed present.
            unsafe {
                bitxor_inplace_avx2(dst, src);
            }
            return;
        }
    }
    bitxor_inplace_scalar(dst, src);
}

/// Reference kernel: safe, portable, LLVM-vectorized to SSE2 on x86_64. The A/B baseline.
#[inline]
pub fn bitxor_inplace_scalar(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] ^= src[i];
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bitxor_inplace_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256, _mm256_xor_si256};
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
                _mm256_xor_si256(a, b),
            );
            offset += 32;
        }
    }
    for i in full..n {
        dst[i] ^= src[i];
    }
}

/// `dst[i] = !src[i]` over the overlapping prefix (`min` length). `BITOP NOT`'s kernel.
/// A read-one-write-one stream (fewer streams than AND/OR/XOR, so more compute-bound), which
/// only helps AVX2. Dispatches `avx2 → scalar`. Bit-identical to `for i { dst[i] = !src[i] }`.
#[inline]
pub fn bitnot_into(dst: &mut [u8], src: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 confirmed present.
            unsafe {
                bitnot_into_avx2(dst, src);
            }
            return;
        }
    }
    bitnot_into_scalar(dst, src);
}

/// Reference kernel: safe, portable, LLVM-vectorized to SSE2 on x86_64. The A/B baseline.
#[inline]
pub fn bitnot_into_scalar(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] = !src[i];
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bitnot_into_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{
        __m256i, _mm256_loadu_si256, _mm256_set1_epi8, _mm256_storeu_si256, _mm256_xor_si256,
    };
    let n = dst.len().min(src.len());
    let full = n / 32 * 32;
    // SAFETY: AVX2 guaranteed by the caller. Every load/store spans `[offset, offset+32)` with
    // `offset + 32 <= full <= n <= {dst,src}.len()`, so all accesses stay inside both slices.
    unsafe {
        let ones = _mm256_set1_epi8(-1); // 0xFF lanes; `s ^ 0xFF == !s`
        let mut offset = 0usize;
        while offset < full {
            let s = _mm256_loadu_si256(src.as_ptr().add(offset) as *const __m256i);
            _mm256_storeu_si256(
                dst.as_mut_ptr().add(offset) as *mut __m256i,
                _mm256_xor_si256(s, ones),
            );
            offset += 32;
        }
    }
    for i in full..n {
        dst[i] = !src[i];
    }
}

/// In-place unsigned byte max `dst[i] = max(dst[i], src[i])` over the overlapping prefix.
/// This is the HLL dense register merge (`PFMERGE` / multi-key `PFCOUNT`). LLVM already lowers
/// the scalar `(*dst).max(src)` loop to SSE2 `pmaxub` (16 B/instr); the explicit AVX2
/// `_mm256_max_epu8` (32 B/instr) is the wider sibling — same win as `bitand_inplace` over the
/// L1/L2-resident 16384-register array. Dispatches `avx2 → scalar`. Bit-identical to the loop.
#[inline]
pub fn max_bytes_inplace(dst: &mut [u8], src: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: avx2 confirmed present.
            unsafe {
                max_bytes_inplace_avx2(dst, src);
            }
            return;
        }
    }
    max_bytes_inplace_scalar(dst, src);
}

/// Reference kernel: safe, portable. LLVM lowers this to SSE2 `pmaxub` on x86_64. The A/B baseline.
#[inline]
pub fn max_bytes_inplace_scalar(dst: &mut [u8], src: &[u8]) {
    let n = dst.len().min(src.len());
    for i in 0..n {
        dst[i] = dst[i].max(src[i]);
    }
}

/// # Safety
/// The CPU must support `avx2`. Callers must check `is_x86_feature_detected!("avx2")`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn max_bytes_inplace_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::{
        __m256i, _mm256_loadu_si256, _mm256_max_epu8, _mm256_storeu_si256,
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
                _mm256_max_epu8(a, b),
            );
            offset += 32;
        }
    }
    for i in full..n {
        dst[i] = dst[i].max(src[i]);
    }
}

/// One-pass build of `out[i] = a[i] OP b[i]` for `i in 0..min(a,b)` — the 2-operand `BITOP`
/// shape (`AND`/`OR`/`XOR`) where the result is a fresh `Vec`, not an in-place fold. Same
/// three memory streams as the LLVM-SSE2 `zip().map().collect()` it replaces, but AVX2-wide
/// (32 B/instr) with no extra memset/copy pass (writes straight into the `Vec`'s uninitialised
/// capacity). Bit-identical to `a.iter().zip(b).map(|(x,y)| x OP y).collect()`. `OP`: 0=AND,
/// 1=OR, 2=XOR.
#[inline]
fn binop_collect<const OP: u8>(a: &[u8], b: &[u8]) -> Vec<u8> {
    let n = a.len().min(b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            let mut out = Vec::<u8>::with_capacity(n);
            // SAFETY: avx2 present; the kernel writes exactly `n` bytes into the first `n`
            // slots of `out`'s spare capacity (cap >= n), after which `set_len(n)` marks
            // them initialised.
            unsafe {
                binop_collect_avx2::<OP>(&a[..n], &b[..n], out.as_mut_ptr());
                out.set_len(n);
            }
            return out;
        }
    }
    match OP {
        0 => a[..n].iter().zip(&b[..n]).map(|(x, y)| x & y).collect(),
        1 => a[..n].iter().zip(&b[..n]).map(|(x, y)| x | y).collect(),
        _ => a[..n].iter().zip(&b[..n]).map(|(x, y)| x ^ y).collect(),
    }
}

/// `AND` build. See [`binop_collect`].
#[inline]
pub fn bitand_collect(a: &[u8], b: &[u8]) -> Vec<u8> {
    binop_collect::<0>(a, b)
}
/// `OR` build. See [`binop_collect`].
#[inline]
pub fn bitor_collect(a: &[u8], b: &[u8]) -> Vec<u8> {
    binop_collect::<1>(a, b)
}
/// `XOR` build. See [`binop_collect`].
#[inline]
pub fn bitxor_collect(a: &[u8], b: &[u8]) -> Vec<u8> {
    binop_collect::<2>(a, b)
}

/// # Safety
/// `avx2` must be present; `a.len() == b.len() == n`; `dst` must point to at least `n` writable
/// bytes (the caller `set_len(n)`s afterward). Writes exactly `n` bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn binop_collect_avx2<const OP: u8>(a: &[u8], b: &[u8], dst: *mut u8) {
    use std::arch::x86_64::{
        __m256i, _mm256_and_si256, _mm256_loadu_si256, _mm256_or_si256, _mm256_storeu_si256,
        _mm256_xor_si256,
    };
    let n = a.len();
    let full = n / 32 * 32;
    // SAFETY: caller guarantees avx2 + `dst` has >= n writable bytes and a/b have n readable
    // bytes; every load/store spans `[off, off+32) ⊆ [0, full) ⊆ [0, n)`.
    unsafe {
        let mut off = 0usize;
        while off < full {
            let av = _mm256_loadu_si256(a.as_ptr().add(off) as *const __m256i);
            let bv = _mm256_loadu_si256(b.as_ptr().add(off) as *const __m256i);
            let rv = match OP {
                0 => _mm256_and_si256(av, bv),
                1 => _mm256_or_si256(av, bv),
                _ => _mm256_xor_si256(av, bv),
            };
            _mm256_storeu_si256(dst.add(off) as *mut __m256i, rv);
            off += 32;
        }
        for i in full..n {
            let r = match OP {
                0 => a[i] & b[i],
                1 => a[i] | b[i],
                _ => a[i] ^ b[i],
            };
            *dst.add(i) = r;
        }
    }
}

// ───────────────────────────── CRC-64/Jones (Redis) ─────────────────────────────
//
// Redis's DUMP/RESTORE/RDB checksum: CRC-64/Jones, poly 0xAD93D23594C935A9, refin=refout=true,
// init=0, xorout=0 (check("123456789") = 0xe9c6d914c4b8d9ca). fr-persist ships a slice-by-16 table
// impl (`crc64_redis`). This adds a PCLMULQDQ carry-less-fold kernel; the scalar bit loop here is
// the reference + fallback, and `fr-persist`'s differential test gates any wiring
// (`crc64(x) == crc64_redis(x)` for all inputs), so a wrong fold can never ship — the safe failure
// mode is a parked, unwired kernel.

const CRC64_POLY: u64 = 0xAD93_D235_94C9_35A9; // low 64 of the degree-64 poly (x^64 + this)
const CRC64_POLY_REFLECTED: u64 = crc64_reflect_u64(CRC64_POLY);

const fn crc64_reflect_u64(mut v: u64) -> u64 {
    let mut r = 0u64;
    let mut i = 0;
    while i < 64 {
        r = (r << 1) | (v & 1);
        v >>= 1;
        i += 1;
    }
    r
}

/// `x^n mod P(x)` in forward (non-reflected) bit order (bit i = coefficient of x^i).
const fn crc64_x_pow_mod_p(mut n: u32) -> u64 {
    let mut r: u64 = 1; // x^0
    while n > 0 {
        let carry = r >> 63; // coefficient of x^63 → x^64 after the shift
        r <<= 1;
        if carry != 0 {
            r ^= CRC64_POLY; // x^64 ≡ low64(P)
        }
        n -= 1;
    }
    r
}

/// Fold constant for the low 64 bits of the accumulator (multiplied via `clmul` imm `0x00`):
/// `reflect(x^191 mod P)`. The exponents (191 for the low half, 127 for the high half) and the
/// scalar final reduction were determined by an exhaustive software-`clmul` model of this exact
/// fold, verified `== crc64_scalar` for all lengths `0..=1000` × 3 seeds and the CRC-64/Jones check
/// value `0xe9c6d914c4b8d9ca`. Because the reduction is `crc64_scalar` over the folded 16 bytes,
/// no Barrett constant is needed.
const CRC64_FOLD_K_LO: u64 = crc64_reflect_u64(crc64_x_pow_mod_p(191));
/// Fold constant for the high 64 bits (multiplied via `clmul` imm `0x11`): `reflect(x^127 mod P)`.
const CRC64_FOLD_K_HI: u64 = crc64_reflect_u64(crc64_x_pow_mod_p(127));

/// Fold constants for a **4-block (512-bit) advance**, used by the fold-by-4 main loop that keeps
/// four independent 128-bit accumulators so the ~4-cycle-latency `PCLMULQDQ` on the fold's critical
/// path is hidden behind independent work (a single-accumulator fold is latency-bound). Same
/// reflected form as the single-block constants, exponents shifted up by three blocks: a `D`-block
/// advance uses `(x^(63 + D·128), x^(-1 + D·128))`, so `D=1` gives `(191, 127)` and `D=4` gives
/// `(575, 511)`. A software-`clmul` model verified fold-by-4 with these constants `== crc64_scalar`
/// for all lengths `0..=1200`; the exhaustive dispatch test then gates `0..=2048`.
const CRC64_FOLD4_K_LO: u64 = crc64_reflect_u64(crc64_x_pow_mod_p(575));
const CRC64_FOLD4_K_HI: u64 = crc64_reflect_u64(crc64_x_pow_mod_p(511));

/// Reference kernel: safe, portable bit-wise reflected CRC-64/Jones. The oracle every fold arm is
/// tested against, and the fallback on non-`x86_64` / non-`pclmulqdq` hosts.
#[inline]
pub fn crc64_scalar(mut crc: u64, data: &[u8]) -> u64 {
    for &byte in data {
        crc ^= byte as u64;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (CRC64_POLY_REFLECTED & 0u64.wrapping_sub(crc & 1));
            bit += 1;
        }
    }
    crc
}

/// CRC-64/Jones of `data` (init 0), dispatching to PCLMULQDQ where available.
///
/// Bit-identical to `crc64_scalar(0, data)` for every input (proven by an exhaustive software-clmul
/// model and by the crate's differential test). Byte-exact to `fr_persist::crc64_redis` — the
/// gate `fr-persist` runs before wiring.
#[inline]
pub fn crc64(data: &[u8]) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // Below ~4 blocks the fixed reduction cost (a scalar CRC over 16 bytes) outweighs the fold;
        // the null-gated bench places the crossover, and the scalar path is byte-identical anyway.
        if data.len() >= 64 && std::arch::is_x86_feature_detected!("pclmulqdq") {
            // SAFETY: pclmulqdq (and SSE2 baseline) confirmed present; data.len() >= 16.
            return unsafe { crc64_pclmul(data) };
        }
    }
    crc64_scalar(0, data)
}

/// One reflected carry-less fold: `(acc.lo · k.lo) ^ (acc.hi · k.hi) ^ next`. A macro rather than a
/// fn so it always inlines inside the `#[target_feature]` kernels — `#[inline(always)]` is rejected
/// on `#[target_feature]` fns, and a non-inlined call per fold would erase the fold-by-4 win. The
/// caller's `use` brings `_mm_clmulepi64_si128` / `_mm_xor_si128` into scope.
#[cfg(target_arch = "x86_64")]
macro_rules! crc64_fold {
    ($acc:expr, $k:expr, $next:expr) => {{
        let lo = _mm_clmulepi64_si128($acc, $k, 0x00);
        let hi = _mm_clmulepi64_si128($acc, $k, 0x11);
        _mm_xor_si128(_mm_xor_si128(lo, hi), $next)
    }};
}

/// PCLMULQDQ carry-less **fold-by-4**. Folds 16 bytes per accumulator per step into four
/// independent 128-bit accumulators, so the ~4-cycle `PCLMULQDQ` latency on the fold's critical
/// path is hidden behind the other three lanes (a single accumulator is latency-bound and leaves
/// the unit idle). The four accumulators are then collapsed into one 128-bit residue with three
/// single-block folds — `r = a0·x^(3·128) ^ a1·x^(2·128) ^ a2·x^128 ^ a3` — after which the residue
/// and the `< 16`-byte tail reduce via the scalar CRC (no Barrett constant needed). Below 8 blocks
/// the parallel loop cannot amortize its combine, so it folds by one (byte-identical, no regression
/// on 64..127-byte inputs). Verified `== crc64_scalar` by the exhaustive dispatch test.
///
/// # Safety
/// The CPU must support `pclmulqdq`; `data.len() >= 16`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "pclmulqdq,sse2")]
unsafe fn crc64_pclmul(data: &[u8]) -> u64 {
    use std::arch::x86_64::{
        __m128i, _mm_clmulepi64_si128, _mm_loadu_si128, _mm_set_epi64x, _mm_storeu_si128,
        _mm_xor_si128,
    };

    let blocks = data.len() / 16;
    let mut folded = [0u8; 16];

    // SAFETY: all intrinsics are pclmulqdq/sse2, guaranteed by the caller. Every load reads 16 bytes
    // at an offset `< blocks*16 <= data.len()`; the store targets a local 16-byte array whose size
    // matches the register width.
    unsafe {
        // k = [high = K_HI, low = K_LO]; imm 0x00 = acc.lo·K_LO, imm 0x11 = acc.hi·K_HI.
        let k1 = _mm_set_epi64x(CRC64_FOLD_K_HI as i64, CRC64_FOLD_K_LO as i64);
        let acc = if blocks >= 8 {
            let k4 = _mm_set_epi64x(CRC64_FOLD4_K_HI as i64, CRC64_FOLD4_K_LO as i64);
            let mut a0 = _mm_loadu_si128(data.as_ptr() as *const __m128i);
            let mut a1 = _mm_loadu_si128(data.as_ptr().add(16) as *const __m128i);
            let mut a2 = _mm_loadu_si128(data.as_ptr().add(32) as *const __m128i);
            let mut a3 = _mm_loadu_si128(data.as_ptr().add(48) as *const __m128i);
            let mut i = 4usize;
            while i + 4 <= blocks {
                // Four independent loads + folds — no cross-lane dependency, so the units stay busy.
                let n0 = _mm_loadu_si128(data.as_ptr().add(i * 16) as *const __m128i);
                let n1 = _mm_loadu_si128(data.as_ptr().add((i + 1) * 16) as *const __m128i);
                let n2 = _mm_loadu_si128(data.as_ptr().add((i + 2) * 16) as *const __m128i);
                let n3 = _mm_loadu_si128(data.as_ptr().add((i + 3) * 16) as *const __m128i);
                a0 = crc64_fold!(a0, k4, n0);
                a1 = crc64_fold!(a1, k4, n1);
                a2 = crc64_fold!(a2, k4, n2);
                a3 = crc64_fold!(a3, k4, n3);
                i += 4;
            }
            // Collapse the four accumulators (a0 leads a3 by three blocks) into one residue.
            let mut r = a0;
            r = crc64_fold!(r, k1, a1);
            r = crc64_fold!(r, k1, a2);
            r = crc64_fold!(r, k1, a3);
            // Fold any full blocks past the last group of four (0..3 of them).
            while i < blocks {
                let next = _mm_loadu_si128(data.as_ptr().add(i * 16) as *const __m128i);
                r = crc64_fold!(r, k1, next);
                i += 1;
            }
            r
        } else {
            let mut acc = _mm_loadu_si128(data.as_ptr() as *const __m128i); // first block; init crc = 0
            let mut i = 1usize;
            while i < blocks {
                let next = _mm_loadu_si128(data.as_ptr().add(i * 16) as *const __m128i);
                acc = crc64_fold!(acc, k1, next);
                i += 1;
            }
            acc
        };
        _mm_storeu_si128(folded.as_mut_ptr() as *mut __m128i, acc);
    }

    let crc = crc64_scalar(0, &folded);
    crc64_scalar(crc, &data[blocks * 16..])
}

/// Pre-`fold-by-4` reference: the single-accumulator fold, retained **only** as the ORIG arm of the
/// same-binary A/B in `benches/crc64.rs` (methodology §3 — keep the pre-change impl bench-faithful).
/// Byte-identical to [`crc64`] for every input; not on any production path.
#[doc(hidden)]
#[must_use]
pub fn crc64_fold1_reference(data: &[u8]) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        if data.len() >= 64 && std::arch::is_x86_feature_detected!("pclmulqdq") {
            // SAFETY: pclmulqdq (and SSE2 baseline) confirmed present; data.len() >= 16.
            return unsafe { crc64_pclmul_fold1(data) };
        }
    }
    crc64_scalar(0, data)
}

/// The single-accumulator fold body backing [`crc64_fold1_reference`]. Identical math to the shipped
/// kernel's `< 8`-block arm.
///
/// # Safety
/// The CPU must support `pclmulqdq`; `data.len() >= 16`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "pclmulqdq,sse2")]
unsafe fn crc64_pclmul_fold1(data: &[u8]) -> u64 {
    use std::arch::x86_64::{
        __m128i, _mm_clmulepi64_si128, _mm_loadu_si128, _mm_set_epi64x, _mm_storeu_si128,
        _mm_xor_si128,
    };
    let blocks = data.len() / 16;
    let mut folded = [0u8; 16];
    // SAFETY: as `crc64_pclmul` — pclmulqdq/sse2 present, loads in bounds, local store.
    unsafe {
        let k1 = _mm_set_epi64x(CRC64_FOLD_K_HI as i64, CRC64_FOLD_K_LO as i64);
        let mut acc = _mm_loadu_si128(data.as_ptr() as *const __m128i);
        let mut i = 1usize;
        while i < blocks {
            let next = _mm_loadu_si128(data.as_ptr().add(i * 16) as *const __m128i);
            acc = crc64_fold!(acc, k1, next);
            i += 1;
        }
        _mm_storeu_si128(folded.as_mut_ptr() as *mut __m128i, acc);
    }
    let crc = crc64_scalar(0, &folded);
    crc64_scalar(crc, &data[blocks * 16..])
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

    /// The PCLMULQDQ CRC dispatch must equal the scalar reference for EVERY input — the fold body,
    /// the single-block case, every tail remainder (0..16), and every alignment — and must hit the
    /// CRC-64/Jones check value. A wrong CRC silently corrupts DUMP/RESTORE/RDB, so this is
    /// exhaustive and the crate's most safety-critical test.
    #[test]
    fn crc64_pclmul_matches_scalar_and_check_value() {
        use super::{crc64, crc64_fold1_reference, crc64_scalar};
        assert_eq!(crc64(b"123456789"), 0xe9c6_d914_c4b8_d9ca, "CRC-64/Jones check value");
        let mut buf = vec![0u8; 2048];
        for seed in [1u64, 0xdead_beef, 0x0f0f_0f0f_f0f0_f0f0] {
            fill(&mut buf, seed);
            for len in 0..=2048usize {
                let scalar = crc64_scalar(0, &buf[..len]);
                // The shipped fold-by-4 dispatch AND the retained fold-by-1 A/B reference must both
                // equal the scalar oracle — a wrong 4-block constant, a broken combine, or a stale
                // reference all fail here (the 8-block boundary, every 0..3 remainder, and the
                // 4..7-block fold-by-1 fallback are all exercised across this length sweep).
                assert_eq!(crc64(&buf[..len]), scalar, "fold4 != scalar at len={len} seed={seed}");
                assert_eq!(
                    crc64_fold1_reference(&buf[..len]),
                    scalar,
                    "fold1 reference != scalar at len={len} seed={seed}"
                );
            }
        }
        // Unaligned starts exercise the unaligned 16-byte loads.
        for start in 0..16usize {
            for len in [0usize, 15, 16, 31, 63, 64, 65, 127, 300] {
                if start + len > buf.len() {
                    continue;
                }
                let s = &buf[start..start + len];
                assert_eq!(crc64(s), crc64_scalar(0, s), "start={start} len={len}");
                assert_eq!(crc64(s), crc64_fold1_reference(s), "fold4 != fold1 start={start} len={len}");
            }
        }
    }

    /// `common_prefix_len` must equal the scalar oracle for every input: the difference walked
    /// across every position (incl. the 16- and 32-byte lane boundaries and the tail), equal
    /// slices, unequal lengths, and every alignment. A wrong return value would silently change LZF
    /// output, so this is exhaustive.
    #[test]
    fn common_prefix_len_matches_scalar_exhaustively() {
        use super::{common_prefix_len, common_prefix_len_scalar};
        fn oracle(a: &[u8], b: &[u8]) -> usize {
            let n = a.len().min(b.len());
            (0..n).find(|&i| a[i] != b[i]).unwrap_or(n)
        }
        let mut base = vec![0u8; 200];
        fill(&mut base, 0xcafe_f00d_1234_5678);
        for len in 0..=140usize {
            // Identical prefixes of every length: no difference => full min length.
            let a = base[..len].to_vec();
            assert_eq!(common_prefix_len(&a, &a), oracle(&a, &a), "equal len={len}");
            // Single difference walked across every position.
            for pos in 0..len {
                let mut b = a.clone();
                b[pos] ^= 0xff;
                assert_eq!(common_prefix_len(&a, &b), pos, "len={len} pos={pos}");
                assert_eq!(common_prefix_len(&a, &b), common_prefix_len_scalar(&a, &b, len));
            }
        }
        // Unequal lengths: the shorter is a full prefix of the longer.
        let a = vec![0x5au8; 50];
        let b = vec![0x5au8; 20];
        assert_eq!(common_prefix_len(&a, &b), 20);
        assert_eq!(common_prefix_len(&b, &a), 20);
        // Every alignment against a differing tail.
        for astart in 0..34usize {
            for bstart in 0..34usize {
                for len in [1usize, 15, 16, 17, 31, 32, 33, 64, 130] {
                    if astart + len > base.len() || bstart + len > base.len() {
                        continue;
                    }
                    let x = &base[astart..astart + len];
                    let y = &base[bstart..bstart + len];
                    assert_eq!(common_prefix_len(x, y), oracle(x, y), "a={astart} b={bstart} len={len}");
                }
            }
        }
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

    #[test]
    fn bitor_bitxor_match_scalar_and_naive_all_lengths_alignments_and_unequal() {
        use super::{bitor_inplace, bitor_inplace_scalar, bitxor_inplace, bitxor_inplace_scalar};
        let mut da = vec![0u8; 300];
        let mut sa = vec![0u8; 300];
        fill(&mut da, 0x0f0f_1234_5678_9abc);
        fill(&mut sa, 0xf0f0_fedc_ba98_7654);
        for dstart in 0..34usize {
            for sstart in 0..34usize {
                for len in [0usize, 1, 15, 16, 31, 32, 33, 64, 100, 250] {
                    if dstart + len > da.len() || sstart + len > sa.len() {
                        continue;
                    }
                    let base = da[dstart..dstart + len].to_vec();
                    let s = &sa[sstart..sstart + len];
                    // OR: dispatch == scalar == independent naive.
                    let (mut o1, mut o2) = (base.clone(), base.clone());
                    let onaive: Vec<u8> = base.iter().zip(s).map(|(a, b)| a | b).collect();
                    bitor_inplace(&mut o1, s);
                    bitor_inplace_scalar(&mut o2, s);
                    assert_eq!(o1, o2, "OR dispatch!=scalar d={dstart} s={sstart} len={len}");
                    assert_eq!(o1, onaive, "OR wrong d={dstart} s={sstart} len={len}");
                    // XOR: dispatch == scalar == independent naive.
                    let (mut x1, mut x2) = (base.clone(), base.clone());
                    let xnaive: Vec<u8> = base.iter().zip(s).map(|(a, b)| a ^ b).collect();
                    bitxor_inplace(&mut x1, s);
                    bitxor_inplace_scalar(&mut x2, s);
                    assert_eq!(x1, x2, "XOR dispatch!=scalar d={dstart} s={sstart} len={len}");
                    assert_eq!(x1, xnaive, "XOR wrong d={dstart} s={sstart} len={len}");
                }
            }
        }
        // Unequal lengths: only the min prefix is touched.
        let mut d = vec![0xaau8; 40];
        let orig = d.clone();
        bitor_inplace(&mut d, &[0x55u8; 10]);
        assert_eq!(&d[..10], &[0xffu8; 10]);
        assert_eq!(&d[10..], &orig[10..], "OR bytes past src.len() untouched");
        let mut d = vec![0xaau8; 40];
        let orig = d.clone();
        bitxor_inplace(&mut d, &[0xffu8; 10]);
        assert_eq!(&d[..10], &[0x55u8; 10]);
        assert_eq!(&d[10..], &orig[10..], "XOR bytes past src.len() untouched");
    }

    #[test]
    fn bitnot_matches_scalar_and_naive_all_lengths_alignments_and_unequal() {
        use super::{bitnot_into, bitnot_into_scalar};
        let mut sa = vec![0u8; 300];
        fill(&mut sa, 0x2468_ace0_1357_9bdf);
        for dstart in 0..34usize {
            for sstart in 0..34usize {
                for len in [0usize, 1, 15, 16, 31, 32, 33, 64, 100, 250] {
                    if dstart + len > sa.len() || sstart + len > sa.len() {
                        continue;
                    }
                    let s = &sa[sstart..sstart + len];
                    let (mut d1, mut d2) = (vec![0x5au8; len], vec![0x5au8; len]);
                    let naive: Vec<u8> = s.iter().map(|b| !b).collect();
                    bitnot_into(&mut d1, s);
                    bitnot_into_scalar(&mut d2, s);
                    assert_eq!(d1, d2, "NOT dispatch!=scalar d={dstart} s={sstart} len={len}");
                    assert_eq!(d1, naive, "NOT wrong d={dstart} s={sstart} len={len}");
                }
            }
        }
        // Unequal lengths: only the min prefix is written.
        let mut d = vec![0x11u8; 40];
        bitnot_into(&mut d, &[0xf0u8; 10]);
        assert_eq!(&d[..10], &[0x0fu8; 10]);
        assert_eq!(&d[10..], &[0x11u8; 30], "NOT bytes past src.len() untouched");
    }

    #[test]
    fn max_bytes_matches_scalar_and_naive_all_lengths_alignments_and_unequal() {
        use super::{max_bytes_inplace, max_bytes_inplace_scalar};
        let mut da = vec![0u8; 300];
        let mut sa = vec![0u8; 300];
        fill(&mut da, 0x3141_5926_5358_9793);
        fill(&mut sa, 0x2718_2818_2845_9045);
        for dstart in 0..34usize {
            for sstart in 0..34usize {
                for len in [0usize, 1, 15, 16, 31, 32, 33, 64, 100, 250] {
                    if dstart + len > da.len() || sstart + len > sa.len() {
                        continue;
                    }
                    let base = da[dstart..dstart + len].to_vec();
                    let s = &sa[sstart..sstart + len];
                    let (mut d1, mut d2) = (base.clone(), base.clone());
                    let naive: Vec<u8> = base.iter().zip(s).map(|(a, b)| (*a).max(*b)).collect();
                    max_bytes_inplace(&mut d1, s);
                    max_bytes_inplace_scalar(&mut d2, s);
                    assert_eq!(d1, d2, "MAX dispatch!=scalar d={dstart} s={sstart} len={len}");
                    assert_eq!(d1, naive, "MAX wrong d={dstart} s={sstart} len={len}");
                }
            }
        }
        // Unequal lengths: only the min prefix is touched.
        let mut d = vec![0x30u8; 40];
        let orig = d.clone();
        max_bytes_inplace(&mut d, &[0x0fu8, 0x99, 0x30, 0x31, 0x2f]);
        assert_eq!(&d[..5], &[0x30, 0x99, 0x30, 0x31, 0x30]);
        assert_eq!(&d[5..], &orig[5..], "MAX bytes past src.len() untouched");
    }

    #[test]
    fn bit_collect_matches_naive_all_lengths_alignments_and_unequal() {
        use super::{bitand_collect, bitor_collect, bitxor_collect};
        let mut aa = vec![0u8; 320];
        let mut bb = vec![0u8; 320];
        fill(&mut aa, 0x1357_9bdf_2468_ace0);
        fill(&mut bb, 0xfdb9_7531_0eca_8642);
        for astart in 0..34usize {
            for bstart in 0..34usize {
                for len in [0usize, 1, 15, 16, 31, 32, 33, 63, 64, 65, 100, 255] {
                    if astart + len > aa.len() || bstart + len > bb.len() {
                        continue;
                    }
                    let a = &aa[astart..astart + len];
                    let b = &bb[bstart..bstart + len];
                    let and_n: Vec<u8> = a.iter().zip(b).map(|(x, y)| x & y).collect();
                    let or_n: Vec<u8> = a.iter().zip(b).map(|(x, y)| x | y).collect();
                    let xor_n: Vec<u8> = a.iter().zip(b).map(|(x, y)| x ^ y).collect();
                    assert_eq!(bitand_collect(a, b), and_n, "AND a={astart} b={bstart} len={len}");
                    assert_eq!(bitor_collect(a, b), or_n, "OR a={astart} b={bstart} len={len}");
                    assert_eq!(bitxor_collect(a, b), xor_n, "XOR a={astart} b={bstart} len={len}");
                }
            }
        }
        // Unequal lengths: result length is the min.
        assert_eq!(bitand_collect(&[0xff; 40], &[0x0f; 10]), vec![0x0f; 10]);
        assert_eq!(bitxor_collect(&[0xaa; 5], &[0xff; 12]), vec![0x55; 5]);
    }
}
