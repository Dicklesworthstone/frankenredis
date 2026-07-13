#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt::{self, Display};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespFrame {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(Option<Vec<u8>>),
    Array(Option<Vec<RespFrame>>),
    Map(Option<Vec<(RespFrame, RespFrame)>>),
    Push(Vec<RespFrame>),
    Sequence(Vec<RespFrame>),
    /// RESP3 Double type (`,value\r\n`). Stores string representation to allow Eq derive.
    Double(String),
    /// RESP3 Set type (`~count\r\n` followed by elements).
    Set(Option<Vec<RespFrame>>),
    /// RESP3 Verbatim string (`=<len>\r\ntxt:<body>\r\n`). Used for INFO, CLIENT INFO/LIST,
    /// LOLWUT, LATENCY DOCTOR, MEMORY DOCTOR under HELLO 3.
    Verbatim(String),
    /// RESP3 Big Number (`(value\r\n`). Used by the Lua `{big_number=...}`
    /// reply hint; downconverts to a bulk string under RESP2. (frankenredis-h2uga)
    BigNumber(String),
    /// RESP3 Boolean (`#t\r\n` / `#f\r\n`). Upstream `addReplyBool` emits this
    /// under HELLO 3 and downgrades to the integer `:1` / `:0` for a RESP2
    /// client. Produced by the Lua boolean return path under `redis.setresp(3)`
    /// and by `DEBUG PROTOCOL true|false`. (frankenredis-0gz4g)
    Bool(bool),
    /// RESP3 Attribute (`|count\r\n` followed by key/value pairs). An attribute
    /// is metadata that PREFIXES the real reply on the wire — emit it as the
    /// first element of a `Sequence` whose remaining element(s) are the reply
    /// it annotates. RESP3-only (no RESP2 wire form). Used by
    /// `DEBUG PROTOCOL attrib`. (frankenredis-01weh)
    Attribute(Vec<(RespFrame, RespFrame)>),
}

/// Sanitize bytes destined for an inline RESP frame body (`SimpleString`
/// or `Error`). RESP inline frames are terminated by the first `\r\n`,
/// so any embedded `\r` or `\n` in the payload would split the frame
/// and let bytes after the split be re-parsed by the peer as a separate
/// reply — a CRLF-injection / frame-smuggling primitive when the body
/// is built from user-controlled input via `format!()`. Mirrors
/// upstream Redis' `_addReplyErrorFormat` which does
/// `sdsmapchars(s, "\r\n", "  ", 2)` for the same reason.
///
/// Used both for encoding (output) and parsing (input) to ensure
/// roundtrip consistency: `parse(encode(frame)) == frame`.
fn sanitize_inline_body(s: &str) -> String {
    if !s.bytes().any(|b| b == b'\r' || b == b'\n') {
        return s.to_owned();
    }
    s.chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect()
}

/// (frankenredis-itoa2) Two-digit decimal lookup table: `DIGIT_PAIRS[2*k..2*k+2]`
/// is the ASCII for `k` (`00`..`99`). Formatting two digits per iteration halves
/// the loop count and the (compiler-lowered) divide-by-constant operations vs a
/// digit-at-a-time `%10`/`/10` loop, on the universal RESP reply path (every
/// length header + integer reply runs through here).
const fn build_digit_pairs() -> [u8; 200] {
    let mut t = [0u8; 200];
    let mut k = 0usize;
    while k < 100 {
        t[k * 2] = b'0' + (k / 10) as u8;
        t[k * 2 + 1] = b'0' + (k % 10) as u8;
        k += 1;
    }
    t
}
const DIGIT_PAIRS: [u8; 200] = build_digit_pairs();

/// Write the decimal ASCII of `val` into `buf` ending at `buf[end]`, returning
/// the start index. `buf` must be at least 20 bytes and `end == buf.len()`.
/// Two digits per step via [`DIGIT_PAIRS`]. (frankenredis-itoa2)
pub fn write_u64_digits(buf: &mut [u8; 20], end: usize, mut val: u64) -> usize {
    let mut pos = end;
    while val >= 100 {
        let pair = (val % 100) as usize * 2;
        val /= 100;
        pos -= 2;
        buf[pos] = DIGIT_PAIRS[pair];
        buf[pos + 1] = DIGIT_PAIRS[pair + 1];
    }
    if val < 10 {
        pos -= 1;
        buf[pos] = b'0' + val as u8;
    } else {
        let pair = val as usize * 2;
        pos -= 2;
        buf[pos] = DIGIT_PAIRS[pair];
        buf[pos + 1] = DIGIT_PAIRS[pair + 1];
    }
    pos
}

/// Fast integer-to-bytes without format machinery. Writes decimal representation
/// of `n` directly into `out`. Avoids the allocation overhead of write!().
fn push_i64(out: &mut Vec<u8>, n: i64) {
    let (neg, val) = if n < 0 {
        (true, (n as i128).unsigned_abs() as u64)
    } else {
        (false, n as u64)
    };
    let mut buf = [0u8; 20];
    let mut pos = write_u64_digits(&mut buf, 20, val);
    if neg {
        pos -= 1;
        buf[pos] = b'-';
    }
    out.extend_from_slice(&buf[pos..]);
}

/// Fast usize-to-bytes for lengths (always non-negative).
fn push_usize(out: &mut Vec<u8>, n: usize) {
    let mut buf = [0u8; 20];
    let pos = write_u64_digits(&mut buf, 20, n as u64);
    out.extend_from_slice(&buf[pos..]);
}

/// Append a RESP length-prefixed header (`<prefix><n>\r\n`, e.g. `$14\r\n`, `*3\r\n`, `%2\r\n`).
///
/// `FUSED == true` (production) builds the prefix, digits, and `\r\n` terminator right-aligned in
/// one stack buffer and emits them with a SINGLE `extend_from_slice` — the digits are written
/// exactly ONCE (two-at-a-time via `DIGIT_PAIRS`, same core as `write_u64_digits`) directly into
/// their final position, no intermediate copy. This replaces the prior three-call header shape
/// (`extend(prefix)` + `push_usize` + `extend("\r\n")`) on the borrow-encode reply path that fronts
/// every bulk-string / aggregate / map reply (GET / MGET / HGETALL / LRANGE / SMEMBERS / ZRANGE...).
/// `FUSED == false` retains that exact prior shape for the same-binary A/B in
/// `benches/push_len_header_fastpath.rs`; it is not on a production path. Byte-identical: the digit
/// core matches `push_usize`. `n` is a length/count, always non-negative (max 20 u64 digits fit).
#[inline]
fn push_len_header<const FUSED: bool>(out: &mut Vec<u8>, prefix: u8, n: u64) {
    if FUSED {
        // prefix (1) + up to 20 digits (u64::MAX) + "\r\n" (2) = 23 bytes; 24 leaves buf[0] slack.
        let mut buf = [0u8; 24];
        buf[22] = b'\r';
        buf[23] = b'\n';
        let mut val = n;
        let mut pos = 22;
        while val >= 100 {
            let pair = (val % 100) as usize * 2;
            val /= 100;
            pos -= 2;
            buf[pos] = DIGIT_PAIRS[pair];
            buf[pos + 1] = DIGIT_PAIRS[pair + 1];
        }
        if val < 10 {
            pos -= 1;
            buf[pos] = b'0' + val as u8;
        } else {
            let pair = val as usize * 2;
            pos -= 2;
            buf[pos] = DIGIT_PAIRS[pair];
            buf[pos + 1] = DIGIT_PAIRS[pair + 1];
        }
        pos -= 1;
        buf[pos] = prefix;
        out.extend_from_slice(&buf[pos..24]);
    } else {
        out.extend_from_slice(&[prefix]);
        push_usize(out, n as usize);
        out.extend_from_slice(b"\r\n");
    }
}

/// Bench hook for the same-binary A/B in `benches/push_len_header_fastpath.rs`. `FUSED = false`
/// forces the prior three-call header path. Not on a production path.
#[doc(hidden)]
#[inline(never)]
pub fn bench_push_len_header<const FUSED: bool>(out: &mut Vec<u8>, prefix: u8, n: u64) {
    push_len_header::<FUSED>(out, prefix, n)
}

/// Bench hook for `benches/encode_array_reply_fastpath.rs`: encode an array of `count` bulk strings
/// each holding `body` — the shape a populated `RespFrame::Array(Some(..))` of `BulkString`s emits
/// (LRANGE / MGET / SMEMBERS / ZRANGE), which is where the owned-arm `push_len_header` fusion lands.
/// Every RESP header (`*count\r\n` and each `$len\r\n`) goes through `push_len_header`; the bodies
/// are identical in both arms, so only the header path differs — this measures the fusion's win in
/// a realistic reply where real bodies dilute it. `FUSED = false` forces the prior three-call header
/// shape for the same-binary A/B. Not on a production path.
#[doc(hidden)]
#[inline(never)]
pub fn bench_encode_array_reply<const FUSED: bool>(count: usize, body: &[u8], out: &mut Vec<u8>) {
    push_len_header::<FUSED>(out, b'*', count as u64);
    for _ in 0..count {
        push_len_header::<FUSED>(out, b'$', body.len() as u64);
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
}

/// Bench hook that faithfully reproduces `encode_bulk_string_slice`'s `Some` arm (the shipped
/// bab278487 change) so it can be A/B'd in isolation. `FUSED = true` is exactly current production
/// (fused `$<len>\r\n` header via `push_len_header`); `FUSED = false` is the exact pre-bab278487
/// path (`extend("$")` + `push_usize` + `extend("\r\n")`). Both keep the identical `reserve` and
/// body/terminator writes, so the A/B isolates the header shape alone — verifying whether the fused
/// header still wins on a single bulk-string reply (GET/HGET) WITH a body present. Not on a
/// production path.
#[doc(hidden)]
#[inline(never)]
pub fn bench_encode_bulk_string<const FUSED: bool>(bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(1 + decimal_usize_len(bytes.len()) + 2 + bytes.len() + 2);
    if FUSED {
        push_len_header::<true>(out, b'$', bytes.len() as u64);
    } else {
        out.extend_from_slice(b"$");
        push_usize(out, bytes.len());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// Encode a bulk-string reply from borrowed bytes.
///
/// This is byte-identical to `RespFrame::BulkString(...).encode_into*` while
/// letting hot reply paths skip materializing an owned `Vec<u8>` just to hand
/// it to the frame encoder.
pub fn encode_bulk_string_slice(value: Option<&[u8]>, resp3: bool, out: &mut Vec<u8>) {
    match value {
        Some(bytes) => {
            out.reserve(1 + decimal_usize_len(bytes.len()) + 2 + bytes.len() + 2);
            push_len_header::<true>(out, b'$', bytes.len() as u64);
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\r\n");
        }
        None if resp3 => out.extend_from_slice(b"_\r\n"),
        None => out.extend_from_slice(b"$-1\r\n"),
    }
}

/// Write a RESP aggregate header for an array (`*N\r\n`) or, when `resp3_set`,
/// a RESP3 set (`~N\r\n`). Byte-identical to `RespFrame::Array(Some(..))` /
/// `RespFrame::Set(Some(..))`'s header, letting borrow-encoded reply paths emit
/// a collection without materializing a `Vec<RespFrame>`. Pair with one
/// `encode_bulk_string_slice` per element.
pub fn encode_aggregate_header(len: usize, resp3_set: bool, out: &mut Vec<u8>) {
    out.reserve(1 + decimal_usize_len(len) + 2);
    push_len_header::<true>(out, if resp3_set { b'~' } else { b'*' }, len as u64);
}

/// Write a field-value collection header for `pairs` entries: a RESP3 map
/// (`%pairs\r\n`) when `resp3`, else a flat RESP2 array of `2*pairs` elements
/// (`*2pairs\r\n`). Byte-identical to `RespFrame::Map(Some(..))` (RESP3) /
/// `RespFrame::Array(Some(..))` with interleaved field,value (RESP2). Follow with
/// `encode_bulk_string_slice` for each field then value. (frankenredis: HGETALL
/// borrow-encode)
pub fn encode_map_header(pairs: usize, resp3: bool, out: &mut Vec<u8>) {
    if resp3 {
        out.reserve(1 + decimal_usize_len(pairs) + 2);
        push_len_header::<true>(out, b'%', pairs as u64);
    } else {
        let flat = pairs * 2;
        out.reserve(1 + decimal_usize_len(flat) + 2);
        push_len_header::<true>(out, b'*', flat as u64);
    }
}

/// How Redis's `zzlInsertAt` encodes `score` as a sorted-set listpack entry: it renders
/// `d2string(score)` and then lets `lpStringToInt64` re-decide int-vs-string. Deciding it
/// straight from the `f64` lets the two common arms skip that format + decimal re-parse.
///
/// This lives beside [`push_redis_double_ascii`] on purpose: it is a statement *about*
/// `d2string`'s branching, and both the DUMP encoder (`fr-store`) and the RDB-save encoder
/// (`fr-persist`) must agree with it. They previously kept private copies, which silently
/// drifted — `fr-persist` formatted with Rust's `{}` (never scientific) and emitted
/// `"0.0000001"` where upstream emits `"1e-7"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZsetScoreListpackEntry {
    /// `d2string` emits a canonical i64 decimal, so the entry is int-encoded.
    Int(i64),
    /// `d2string`'s output provably is NOT a canonical i64 decimal, so the entry is
    /// string-encoded and the formatted bytes need no re-parse.
    Str,
    /// Integral but outside `double2ll`'s window, so `d2string` falls back to grisu2 —
    /// whose shortest form may still be a canonical i64 decimal. Must be re-parsed.
    Reparse,
}

/// Classify `score` exactly as [`push_redis_double_ascii`] (`d2string`) branches, so the
/// emitted listpack entry is byte-IDENTICAL to formatting the score and re-parsing it.
///
/// The `Reparse` arm is load-bearing, not defensive: above `double2ll`'s window grisu2 still
/// emits a plain canonical decimal for some integral doubles — upstream renders
/// `6917529027641081856` as `"6917529027641082000"` and int-encodes it — so those bytes must
/// go through a `parse_listpack_integer`. Only `Str` may skip it.
pub fn zset_score_listpack_entry(score: f64) -> ZsetScoreListpackEntry {
    // "nan" / "inf" / "-inf" — never a canonical decimal.
    if !score.is_finite() {
        return ZsetScoreListpackEntry::Str;
    }
    // `d2string` special-cases zero BEFORE `double2ll`, and "-0" is non-canonical
    // (`lpStringToInt64` rejects it) while "0" int-encodes. This must precede the integral
    // test below, for which -0.0 is indistinguishable from +0.0.
    if score == 0.0 {
        return if score.is_sign_negative() {
            ZsetScoreListpackEntry::Str
        } else {
            ZsetScoreListpackEntry::Int(0)
        };
    }
    // A non-integral double's shortest grisu2 form always carries a '.' or an 'e': the
    // plain-integer emit branch requires a non-negative decimal exponent, which would make
    // the value integral. So the render can never re-parse as an integer.
    if score.fract() != 0.0 {
        return ZsetScoreListpackEntry::Str;
    }
    // `double2ll`'s window, mirrored bound-for-bound from `push_redis_double_ascii` below.
    // Upstream re-checks `(long long)d == d` afterwards; `fract() == 0.0` above already
    // proves the cast is exact, so the round trip back through f64 is skipped.
    let lo = (-i64::MAX / 2) as f64;
    let hi = (i64::MAX / 2) as f64;
    if score >= lo && score <= hi {
        return ZsetScoreListpackEntry::Int(score as i64);
    }
    ZsetScoreListpackEntry::Reparse
}

/// Append the Redis 7.2 `d2string` ASCII representation of `value` directly
/// into `out`, byte-identical to [`format_redis_double`] but without building an
/// intermediate `String`.
pub fn push_redis_double_ascii(out: &mut Vec<u8>, value: f64) {
    if value.is_nan() {
        out.extend_from_slice(b"nan");
        return;
    }
    if value.is_infinite() {
        out.extend_from_slice(if value > 0.0 { b"inf" } else { b"-inf" });
        return;
    }
    if value == 0.0 {
        out.extend_from_slice(if value.is_sign_negative() {
            b"-0"
        } else {
            b"0"
        });
        return;
    }

    let lo = (-i64::MAX / 2) as f64;
    let hi = (i64::MAX / 2) as f64;
    if value >= lo && value <= hi {
        let truncated = value as i64;
        if truncated as f64 == value {
            push_i64(out, truncated);
            return;
        }
    }

    fpconv_dtoa_into(value, out);
}

/// Write the `push_i64` decimal representation of `n` into the front of `buf`,
/// returning the byte length. Byte-identical to [`push_i64`] (same
/// `write_u64_digits` core, same leading `-`), but into a caller-owned stack
/// slice so a length-prefixed reply can be framed without a memmove.
fn write_i64_to_slice(n: i64, buf: &mut [u8]) -> usize {
    let (neg, val) = if n < 0 {
        (true, (n as i128).unsigned_abs() as u64)
    } else {
        (false, n as u64)
    };
    let mut tmp = [0u8; 20];
    let pos = write_u64_digits(&mut tmp, 20, val);
    let digits = &tmp[pos..];
    let mut i = 0;
    if neg {
        buf[0] = b'-';
        i = 1;
    }
    buf[i..i + digits.len()].copy_from_slice(digits);
    i + digits.len()
}

/// Frame a RESP integer reply (`:<n>\r\n`) into `out`. `FUSED == true` (production) builds the
/// `:` prefix, digits, and `\r\n` terminator in one stack buffer and appends them with a SINGLE
/// `extend_from_slice` — one capacity check + one memcpy — on the universal counter/length reply
/// path (INCR / LLEN / SCARD / EXISTS / DEL / SADD count / ...), instead of three separate extends
/// (`:` prefix, `push_i64`'s own extend, `\r\n`). `FUSED == false` retains the exact prior
/// three-call path for the same-binary A/B in `benches/encode_integer_fastpath.rs`; it is not on a
/// production path. Byte-identical: `write_i64_to_slice` renders the same digits and leading `-` as
/// `push_i64`.
#[inline]
fn encode_integer_reply<const FUSED: bool>(n: i64, out: &mut Vec<u8>) {
    if FUSED {
        // Build ":<n>\r\n" right-aligned in one stack buffer and emit it with a SINGLE
        // extend_from_slice. The digits are written exactly ONCE (two-at-a-time via DIGIT_PAIRS,
        // same core as write_u64_digits) directly into their final position — no intermediate
        // digit copy — with the terminator pre-placed to their right and the ':' prefix (and any
        // '-') filled to their left. ':' + up to 20 signed digits + "\r\n" = 23 bytes; 24 leaves
        // buf[0] as slack so the worst case (i64::MIN) lands at buf[1..24].
        let mut buf = [0u8; 24];
        buf[22] = b'\r';
        buf[23] = b'\n';
        let (neg, mut val) = if n < 0 {
            (true, (n as i128).unsigned_abs() as u64)
        } else {
            (false, n as u64)
        };
        let mut pos = 22;
        while val >= 100 {
            let pair = (val % 100) as usize * 2;
            val /= 100;
            pos -= 2;
            buf[pos] = DIGIT_PAIRS[pair];
            buf[pos + 1] = DIGIT_PAIRS[pair + 1];
        }
        if val < 10 {
            pos -= 1;
            buf[pos] = b'0' + val as u8;
        } else {
            let pair = val as usize * 2;
            pos -= 2;
            buf[pos] = DIGIT_PAIRS[pair];
            buf[pos + 1] = DIGIT_PAIRS[pair + 1];
        }
        if neg {
            pos -= 1;
            buf[pos] = b'-';
        }
        pos -= 1;
        buf[pos] = b':';
        out.extend_from_slice(&buf[pos..24]);
    } else {
        out.extend_from_slice(b":");
        push_i64(out, n);
        out.extend_from_slice(b"\r\n");
    }
}

/// Bench hook for the same-binary A/B in `benches/encode_integer_fastpath.rs`. `FUSED = false`
/// forces the prior three-`extend_from_slice` integer-reply path. Not on a production path.
#[doc(hidden)]
#[inline(never)]
pub fn bench_encode_integer<const FUSED: bool>(n: i64, out: &mut Vec<u8>) {
    encode_integer_reply::<FUSED>(n, out)
}

/// Fast d2string cases that format into a fixed stack `buf` (returning the byte
/// length): NaN/±inf, signed zero, and any integer-valued double in the i64
/// fast-range. Byte-identical to the matching arms of [`push_redis_double_ascii`].
/// Returns `None` for a genuinely fractional value, which needs `fpconv`.
fn try_format_redis_double_simple(value: f64, buf: &mut [u8; 24]) -> Option<usize> {
    if value.is_nan() {
        buf[..3].copy_from_slice(b"nan");
        return Some(3);
    }
    if value.is_infinite() {
        return if value > 0.0 {
            buf[..3].copy_from_slice(b"inf");
            Some(3)
        } else {
            buf[..4].copy_from_slice(b"-inf");
            Some(4)
        };
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            buf[..2].copy_from_slice(b"-0");
            Some(2)
        } else {
            buf[0] = b'0';
            Some(1)
        };
    }
    let lo = (-i64::MAX / 2) as f64;
    let hi = (i64::MAX / 2) as f64;
    if value >= lo && value <= hi {
        let truncated = value as i64;
        if truncated as f64 == value {
            return Some(write_i64_to_slice(truncated, buf));
        }
    }
    None
}

/// Encode a Redis double reply directly into `out`: RESP3 Double when `resp3`
/// is true, RESP2 bulk string otherwise. This is the allocation-free score
/// reply primitive for hot zset paths.
pub fn encode_redis_double(value: f64, resp3: bool, out: &mut Vec<u8>) {
    // Fast path — the overwhelmingly common zset scores (integer-valued) plus
    // the special constants format into a stack buffer, so the length-prefixed
    // RESP2 reply can be written header-then-body IN ORDER. The prior code
    // formatted the body into `out` first, then `resize`-zero-filled and
    // `copy_within`-shifted the whole body forward to prepend `$<len>\r\n` — a
    // per-score memmove that dominated multi-score replies (ZMSCORE/ZRANGE
    // WITHSCORES/ZPOPMIN...). Redis's addReplyHumanLongDouble frames the same
    // way (stack buffer, then bulk write). Byte-identical output.
    let mut buf = [0u8; 24];
    if let Some(n) = try_format_redis_double_simple(value, &mut buf) {
        let body = &buf[..n];
        if resp3 {
            out.reserve(1 + n + 2);
            out.push(b',');
        } else {
            out.reserve(1 + 2 + n + 2 + 2);
            out.push(b'$');
            push_usize(out, n);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
        return;
    }

    // Fractional fallback: fpconv writes into `out`, so keep the format-then-
    // frame path for these rarer values (unchanged, byte-exact).
    if resp3 {
        out.reserve(27);
        out.extend_from_slice(b",");
        push_redis_double_ascii(out, value);
        out.extend_from_slice(b"\r\n");
        return;
    }

    out.reserve(64);
    let body_start = out.len();
    push_redis_double_ascii(out, value);
    let body_end = out.len();
    let body_len = body_end - body_start;

    let mut digits = [0u8; 20];
    let digit_start = write_u64_digits(&mut digits, 20, body_len as u64);
    let digit_len = 20 - digit_start;
    let header_len = 1 + digit_len + 2;

    out.resize(body_end + header_len + 2, 0);
    out.copy_within(body_start..body_end, body_start + header_len);
    out[body_start] = b'$';
    out[body_start + 1..body_start + 1 + digit_len].copy_from_slice(&digits[digit_start..]);
    out[body_start + 1 + digit_len..body_start + header_len].copy_from_slice(b"\r\n");
    out[body_start + header_len + body_len..body_start + header_len + body_len + 2]
        .copy_from_slice(b"\r\n");
}

// (frankenredis-e4fu8) Branchless decimal digit count via ilog10. These run on the
// reply hot path: every integer reply's size + every bulk-string/array/map header's
// reserve sizing. `ilog10` lowers to a leading-zeros + tiny-table sequence (a handful
// of instructions), replacing a data-dependent div-by-10 loop where each `/= 10` is
// ~20-40 cycles and a u64 takes up to 19 iterations. Byte-identical to the old loops
// for every input, including 0 (the loop returned 1; ilog10 is undefined at 0 so we
// special-case it). Verified exhaustively at the digit boundaries by the test below.
#[inline]
fn decimal_u64_len(n: u64) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

#[inline]
fn decimal_usize_len(n: usize) -> usize {
    if n == 0 { 1 } else { n.ilog10() as usize + 1 }
}

fn decimal_i64_len(n: i64) -> usize {
    if n < 0 {
        1 + decimal_u64_len((n as i128).unsigned_abs() as u64)
    } else {
        decimal_u64_len(n as u64)
    }
}

fn push_inline_sanitized(out: &mut Vec<u8>, body: &[u8]) {
    let needs_sanitize = body.iter().any(|&b| b == b'\r' || b == b'\n');
    if !needs_sanitize {
        out.extend_from_slice(body);
        return;
    }
    out.reserve(body.len());
    for &b in body {
        if b == b'\r' || b == b'\n' {
            out.push(b' ');
        } else {
            out.push(b);
        }
    }
}

impl RespFrame {
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len_hint().unwrap_or(0));
        self.encode_into(&mut out);
        out
    }

    fn encoded_len_hint(&self) -> Option<usize> {
        match self {
            Self::SimpleString(s) | Self::Error(s) | Self::Double(s) | Self::BigNumber(s) => {
                1usize.checked_add(s.len())?.checked_add(2)
            }
            Self::Integer(n) => 1usize.checked_add(decimal_i64_len(*n))?.checked_add(2),
            // `#t\r\n` / `#f\r\n` and the RESP2 `:1\r\n` / `:0\r\n` downgrade
            // are all 4 bytes.
            Self::Bool(_) => Some(4),
            // `|count\r\n` header only; pairs add their own hints.
            Self::Attribute(_) => None,
            Self::BulkString(None) => Some(5),
            Self::BulkString(Some(bytes)) => 1usize
                .checked_add(decimal_usize_len(bytes.len()))?
                .checked_add(2)?
                .checked_add(bytes.len())?
                .checked_add(2),
            Self::Array(None) => Some(5),
            Self::Array(Some(frames)) | Self::Set(Some(frames)) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(frames.len()))?
                    .checked_add(2)?;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Map(None) => Some(5),
            Self::Map(Some(entries)) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(entries.len()))?
                    .checked_add(2)?;
                for (key, value) in entries {
                    len = len.checked_add(key.encoded_len_hint()?)?;
                    len = len.checked_add(value.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Push(frames) => {
                let mut len = 1usize
                    .checked_add(decimal_usize_len(frames.len()))?
                    .checked_add(2)?;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Sequence(frames) => {
                let mut len = 0usize;
                for frame in frames {
                    len = len.checked_add(frame.encoded_len_hint()?)?;
                }
                Some(len)
            }
            Self::Set(None) => Some(5),
            Self::Verbatim(s) => {
                let body_len = s.len().checked_add(4)?;
                1usize
                    .checked_add(decimal_usize_len(body_len))?
                    .checked_add(2)?
                    .checked_add(body_len)?
                    .checked_add(2)
            }
        }
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::SimpleString(s) => {
                out.extend_from_slice(b"+");
                push_inline_sanitized(out, s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Error(s) => {
                out.extend_from_slice(b"-");
                push_inline_sanitized(out, s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Integer(n) => encode_integer_reply::<true>(*n, out),
            Self::BulkString(None) => out.extend_from_slice(b"$-1\r\n"),
            Self::BulkString(Some(bytes)) => {
                out.extend_from_slice(b"$");
                push_usize(out, bytes.len());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Array(None) => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Map(None) => out.extend_from_slice(b"%-1\r\n"),
            Self::Map(Some(entries)) => {
                out.extend_from_slice(b"%");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into(out);
                    value.encode_into(out);
                }
            }
            Self::Push(frames) => {
                out.extend_from_slice(b">");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Sequence(frames) => {
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Double(s) => {
                out.extend_from_slice(b",");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::BigNumber(s) => {
                out.extend_from_slice(b"(");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Bool(b) => {
                out.extend_from_slice(if *b { b"#t\r\n" } else { b"#f\r\n" });
            }
            Self::Attribute(entries) => {
                out.extend_from_slice(b"|");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into(out);
                    value.encode_into(out);
                }
            }
            Self::Set(None) => out.extend_from_slice(b"~-1\r\n"),
            Self::Set(Some(frames)) => {
                out.extend_from_slice(b"~");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into(out);
                }
            }
            Self::Verbatim(s) => {
                // RESP3 verbatim string: =<len>\r\ntxt:<body>\r\n
                // len includes the "txt:" prefix (4 bytes)
                out.extend_from_slice(b"=");
                push_usize(out, s.len() + 4);
                out.extend_from_slice(b"\r\ntxt:");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
        }
    }

    /// Encode this frame to RESP3 wire bytes. Identical to [`encode_into`]
    /// except that every null reply uses the RESP3 null type `_\r\n` — redis
    /// 7.2 emits `_` for all nulls under `HELLO 3`, never the RESP2 `$-1` /
    /// `*-1` / `~-1` / `%-1`. Container frames recurse through this method so
    /// nested nulls (e.g. `MGET k missing`, the XPENDING summary's null
    /// consumers field) are promoted too. Scalar and already-RESP3 types
    /// (Double, Verbatim, populated Set/Map) encode identically to
    /// [`encode_into`], so this only diverges on the null leaves.
    ///
    /// [`encode_into`]: Self::encode_into
    pub fn encode_into_resp3(&self, out: &mut Vec<u8>) {
        match self {
            Self::BulkString(None) | Self::Array(None) | Self::Map(None) | Self::Set(None) => {
                out.extend_from_slice(b"_\r\n");
            }
            Self::Array(Some(frames)) => {
                out.extend_from_slice(b"*");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Map(Some(entries)) => {
                out.extend_from_slice(b"%");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into_resp3(out);
                    value.encode_into_resp3(out);
                }
            }
            Self::Attribute(entries) => {
                out.extend_from_slice(b"|");
                push_usize(out, entries.len());
                out.extend_from_slice(b"\r\n");
                for (key, value) in entries {
                    key.encode_into_resp3(out);
                    value.encode_into_resp3(out);
                }
            }
            Self::Set(Some(frames)) => {
                out.extend_from_slice(b"~");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Push(frames) => {
                out.extend_from_slice(b">");
                push_usize(out, frames.len());
                out.extend_from_slice(b"\r\n");
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            Self::Sequence(frames) => {
                for frame in frames {
                    frame.encode_into_resp3(out);
                }
            }
            // Scalars (SimpleString/Error/Integer/Double/Verbatim/populated
            // BulkString) carry no nulls and encode identically.
            _ => self.encode_into(out),
        }
    }

    /// Create a RESP3 Double frame from an f64, formatted exactly as
    /// vendored Redis 7.2.4 `addReplyDouble`/`d2string` would (so RESP3
    /// `,<value>\r\n` is byte-identical to upstream). (frankenredis-sk4ss)
    #[must_use]
    pub fn double_from_f64(v: f64) -> Self {
        Self::Double(format_redis_double(v))
    }
}

/// Format an f64 exactly as vendored Redis 7.2.4 util.c::d2string does,
/// the canonical conversion behind ZSCORE / ZADD INCR / GEODIST and the
/// RESP3 Double type. d2string special-cases nan/inf/±0, fast-paths
/// exact integers (double2ll → ll2string), and otherwise emits the
/// Grisu shortest-roundtrip form via deps/fpconv/fpconv_dtoa.c
/// ::emit_digits. Rust's `{:e}` yields the same shortest significant
/// digits and base-10 exponent that fpconv derives, so we re-lay them
/// out with fpconv's exact fixed-vs-scientific rules — keyed on K (the
/// power of the LAST digit), not on the leading exponent. Verified
/// byte-exact against the oracle across 437 magnitudes. (frankenredis-sk4ss)
#[must_use]
pub fn format_redis_double(value: f64) -> String {
    // Faithful port of util.c::d2string (the path addReplyHumanLongDouble /
    // addReplyDouble take for RESP2 scores): special-case nan/inf/±0, take the
    // exact-integer ll2string fast path ONLY inside the window upstream's
    // util.c::double2ll uses — `(double)(±LLONG_MAX/2)`, which rounds to ±2^62
    // (4611686018427387904), NOT ±2^52 — otherwise grisu2 via fpconv_dtoa.
    // The bound matters: the oracle renders 1e16/1e18/4e18 as plain decimals and
    // only goes scientific at 5e18 ("5e+18"). (frankenredis-sk4ss)
    let mut out = Vec::with_capacity(24);
    push_redis_double_ascii(&mut out, value);
    String::from_utf8(out).expect("ascii")
}

// ───────────────────────── fpconv_dtoa (grisu2) ────────────────────────────
//
// Faithful safe-Rust port of redis deps/fpconv/fpconv_dtoa.c (Florian Loitsch's
// Grisu2), so ZSET-score / GEODIST / RESP3-double replies are byte-identical to
// vendored redis 7.2.4 — including the cases where grisu2's shortest digits
// differ in the last place from Rust's Ryū. All unsigned arithmetic uses the
// wrapping ops the C relies on (uint64_t is modular), so the debug build does
// not panic on the deliberate overflows in `multiply`/`generate_digits`.
// (frankenredis fpconv grisu2 port)

const FPCONV_FRACMASK: u64 = 0x000F_FFFF_FFFF_FFFF;
const FPCONV_EXPMASK: u64 = 0x7FF0_0000_0000_0000;
const FPCONV_HIDDENBIT: u64 = 0x0010_0000_0000_0000;
const FPCONV_SIGNMASK: u64 = 0x8000_0000_0000_0000;
const FPCONV_EXPBIAS: i32 = 1023 + 52;

const FPCONV_TENS: [u64; 20] = [
    10_000_000_000_000_000_000,
    1_000_000_000_000_000_000,
    100_000_000_000_000_000,
    10_000_000_000_000_000,
    1_000_000_000_000_000,
    100_000_000_000_000,
    10_000_000_000_000,
    1_000_000_000_000,
    100_000_000_000,
    10_000_000_000,
    1_000_000_000,
    100_000_000,
    10_000_000,
    1_000_000,
    100_000,
    10_000,
    1_000,
    100,
    10,
    1,
];

const FPCONV_NPOWERS: i32 = 87;
const FPCONV_STEPPOWERS: i32 = 8;
const FPCONV_FIRSTPOWER: i32 = -348;
const FPCONV_EXPMAX: i32 = -32;
const FPCONV_EXPMIN: i32 = -60;

/// `(frac, exp)` cached powers of ten (87 entries), verbatim from fpconv_powers.h.
const FPCONV_POWERS: [(u64, i32); 87] = [
    (18054884314459144840, -1220),
    (13451937075301367670, -1193),
    (10022474136428063862, -1166),
    (14934650266808366570, -1140),
    (11127181549972568877, -1113),
    (16580792590934885855, -1087),
    (12353653155963782858, -1060),
    (18408377700990114895, -1034),
    (13715310171984221708, -1007),
    (10218702384817765436, -980),
    (15227053142812498563, -954),
    (11345038669416679861, -927),
    (16905424996341287883, -901),
    (12595523146049147757, -874),
    (9384396036005875287, -847),
    (13983839803942852151, -821),
    (10418772551374772303, -794),
    (15525180923007089351, -768),
    (11567161174868858868, -741),
    (17236413322193710309, -715),
    (12842128665889583758, -688),
    (9568131466127621947, -661),
    (14257626930069360058, -635),
    (10622759856335341974, -608),
    (15829145694278690180, -582),
    (11793632577567316726, -555),
    (17573882009934360870, -529),
    (13093562431584567480, -502),
    (9755464219737475723, -475),
    (14536774485912137811, -449),
    (10830740992659433045, -422),
    (16139061738043178685, -396),
    (12024538023802026127, -369),
    (17917957937422433684, -343),
    (13349918974505688015, -316),
    (9946464728195732843, -289),
    (14821387422376473014, -263),
    (11042794154864902060, -236),
    (16455045573212060422, -210),
    (12259964326927110867, -183),
    (18268770466636286478, -157),
    (13611294676837538539, -130),
    (10141204801825835212, -103),
    (15111572745182864684, -77),
    (11258999068426240000, -50),
    (16777216000000000000, -24),
    (12500000000000000000, 3),
    (9313225746154785156, 30),
    (13877787807814456755, 56),
    (10339757656912845936, 83),
    (15407439555097886824, 109),
    (11479437019748901445, 136),
    (17105694144590052135, 162),
    (12744735289059618216, 189),
    (9495567745759798747, 216),
    (14149498560666738074, 242),
    (10542197943230523224, 269),
    (15709099088952724970, 295),
    (11704190886730495818, 322),
    (17440603504673385349, 348),
    (12994262207056124023, 375),
    (9681479787123295682, 402),
    (14426529090290212157, 428),
    (10748601772107342003, 455),
    (16016664761464807395, 481),
    (11933345169920330789, 508),
    (17782069995880619868, 534),
    (13248674568444952270, 561),
    (9871031767461413346, 588),
    (14708983551653345445, 614),
    (10959046745042015199, 641),
    (16330252207878254650, 667),
    (12166986024289022870, 694),
    (18130221999122236476, 720),
    (13508068024458167312, 747),
    (10064294952495520794, 774),
    (14996968138956309548, 800),
    (11173611982879273257, 827),
    (16649979327439178909, 853),
    (12405201291620119593, 880),
    (9242595204427927429, 907),
    (13772540099066387757, 933),
    (10261342003245940623, 960),
    (15290591125556738113, 986),
    (11392378155556871081, 1013),
    (16975966327722178521, 1039),
    (12648080533535911531, 1066),
];

#[derive(Clone, Copy)]
struct Fp {
    frac: u64,
    exp: i32,
}

fn fpconv_build_fp(d: f64) -> Fp {
    let bits = d.to_bits();
    let mut frac = bits & FPCONV_FRACMASK;
    let mut exp = ((bits & FPCONV_EXPMASK) >> 52) as i32;
    if exp != 0 {
        frac += FPCONV_HIDDENBIT;
        exp -= FPCONV_EXPBIAS;
    } else {
        exp = -FPCONV_EXPBIAS + 1;
    }
    Fp { frac, exp }
}

fn fpconv_normalize(fp: &mut Fp) {
    while fp.frac & FPCONV_HIDDENBIT == 0 {
        fp.frac <<= 1;
        fp.exp -= 1;
    }
    let shift = 64 - 52 - 1;
    fp.frac <<= shift;
    fp.exp -= shift;
}

fn fpconv_get_normalized_boundaries(fp: &Fp) -> (Fp, Fp) {
    let mut upper = Fp {
        frac: (fp.frac << 1) + 1,
        exp: fp.exp - 1,
    };
    while upper.frac & (FPCONV_HIDDENBIT << 1) == 0 {
        upper.frac <<= 1;
        upper.exp -= 1;
    }
    let u_shift = 64 - 52 - 2;
    upper.frac <<= u_shift;
    upper.exp -= u_shift;

    let l_shift = if fp.frac == FPCONV_HIDDENBIT { 2 } else { 1 };
    let mut lower = Fp {
        frac: (fp.frac << l_shift) - 1,
        exp: fp.exp - l_shift,
    };
    lower.frac = lower.frac.wrapping_shl((lower.exp - upper.exp) as u32);
    lower.exp = upper.exp;
    (lower, upper)
}

fn fpconv_multiply(a: &Fp, b: &Fp) -> Fp {
    let lomask: u64 = 0x0000_0000_FFFF_FFFF;
    let ah_bl = (a.frac >> 32).wrapping_mul(b.frac & lomask);
    let al_bh = (a.frac & lomask).wrapping_mul(b.frac >> 32);
    let al_bl = (a.frac & lomask).wrapping_mul(b.frac & lomask);
    let ah_bh = (a.frac >> 32).wrapping_mul(b.frac >> 32);

    let tmp = (ah_bl & lomask)
        .wrapping_add(al_bh & lomask)
        .wrapping_add(al_bl >> 32)
        .wrapping_add(1u64 << 31);

    Fp {
        frac: ah_bh
            .wrapping_add(ah_bl >> 32)
            .wrapping_add(al_bh >> 32)
            .wrapping_add(tmp >> 32),
        exp: a.exp + b.exp + 64,
    }
}

fn fpconv_round_digit(
    digits: &mut [u8],
    ndigits: usize,
    delta: u64,
    mut rem: u64,
    kappa: u64,
    frac: u64,
) {
    while rem < frac
        && delta.wrapping_sub(rem) >= kappa
        && (rem.wrapping_add(kappa) < frac
            || frac.wrapping_sub(rem) > rem.wrapping_add(kappa).wrapping_sub(frac))
    {
        digits[ndigits - 1] = digits[ndigits - 1].wrapping_sub(1);
        rem = rem.wrapping_add(kappa);
    }
}

fn fpconv_generate_digits(
    fp: &Fp,
    upper: &Fp,
    lower: &Fp,
    digits: &mut [u8],
    k: &mut i32,
) -> usize {
    let wfrac = upper.frac.wrapping_sub(fp.frac);
    let mut delta = upper.frac.wrapping_sub(lower.frac);

    let one_exp = upper.exp;
    let one_shift = (-one_exp) as u32;
    let one_frac = 1u64 << one_shift;

    let mut part1 = upper.frac >> one_shift;
    let mut part2 = upper.frac & (one_frac - 1);

    let mut idx = 0usize;
    let mut kappa: i32 = 10;

    // divp walks tens[10]=1_000_000_000 .. upward.
    let mut divp = 10usize;
    while kappa > 0 {
        let div = FPCONV_TENS[divp];
        let digit = (part1 / div) as u32;
        if digit != 0 || idx != 0 {
            digits[idx] = (digit as u8) + b'0';
            idx += 1;
        }
        part1 -= digit as u64 * div;
        kappa -= 1;

        let tmp = (part1 << one_shift).wrapping_add(part2);
        if tmp <= delta {
            *k += kappa;
            fpconv_round_digit(digits, idx, delta, tmp, div.wrapping_shl(one_shift), wfrac);
            return idx;
        }
        divp += 1;
    }

    // tens[18] = 10.
    let mut unit = 18usize;
    loop {
        part2 = part2.wrapping_mul(10);
        delta = delta.wrapping_mul(10);
        kappa -= 1;

        let digit = (part2 >> one_shift) as u32;
        if digit != 0 || idx != 0 {
            digits[idx] = (digit as u8) + b'0';
            idx += 1;
        }
        part2 &= one_frac - 1;
        if part2 < delta {
            *k += kappa;
            fpconv_round_digit(
                digits,
                idx,
                delta,
                part2,
                one_frac,
                wfrac.wrapping_mul(FPCONV_TENS[unit]),
            );
            return idx;
        }
        unit -= 1;
    }
}

fn fpconv_find_cachedpow10(exp: i32, k: &mut i32) -> Fp {
    let one_log_ten = 0.301_029_995_663_981_14_f64;
    let approx = ((-(exp + FPCONV_NPOWERS)) as f64 * one_log_ten) as i32;
    let mut idx = ((approx - FPCONV_FIRSTPOWER) / FPCONV_STEPPOWERS) as usize;
    loop {
        let current = exp + FPCONV_POWERS[idx].1 + 64;
        if current < FPCONV_EXPMIN {
            idx += 1;
            continue;
        }
        if current > FPCONV_EXPMAX {
            idx -= 1;
            continue;
        }
        *k = FPCONV_FIRSTPOWER + (idx as i32) * FPCONV_STEPPOWERS;
        let (frac, e) = FPCONV_POWERS[idx];
        return Fp { frac, exp: e };
    }
}

fn fpconv_grisu2(d: f64, digits: &mut [u8], k: &mut i32) -> usize {
    let mut w = fpconv_build_fp(d);
    let (mut lower, mut upper) = fpconv_get_normalized_boundaries(&w);
    fpconv_normalize(&mut w);

    let mut kk = 0i32;
    let cp = fpconv_find_cachedpow10(upper.exp, &mut kk);

    w = fpconv_multiply(&w, &cp);
    upper = fpconv_multiply(&upper, &cp);
    lower = fpconv_multiply(&lower, &cp);

    lower.frac = lower.frac.wrapping_add(1);
    upper.frac = upper.frac.wrapping_sub(1);

    *k = -kk;
    fpconv_generate_digits(&w, &upper, &lower, digits, k)
}

fn fpconv_emit_digits_into(
    digits: &[u8],
    mut ndigits: usize,
    k: i32,
    neg: bool,
    out: &mut Vec<u8>,
) {
    let start_len = out.len();
    let exp = (k + ndigits as i32 - 1).abs();

    // write plain integer
    if k >= 0 && exp < ndigits as i32 + 7 {
        out.extend_from_slice(&digits[..ndigits]);
        out.resize(start_len + ndigits + k as usize, b'0');
        return;
    }

    // write decimal w/o scientific notation
    if k < 0 && (k > -7 || exp < 4) {
        let offset = ndigits as i32 - k.abs();
        if offset <= 0 {
            let off = (-offset) as usize;
            out.push(b'0');
            out.push(b'.');
            out.resize(start_len + 2 + off, b'0');
            out.extend_from_slice(&digits[..ndigits]);
        } else {
            let off = offset as usize;
            out.extend_from_slice(&digits[..off]);
            out.push(b'.');
            out.extend_from_slice(&digits[off..ndigits]);
        }
        return;
    }

    // write decimal w/ scientific notation
    ndigits = ndigits.min(18 - usize::from(neg));
    out.push(digits[0]);
    if ndigits > 1 {
        out.push(b'.');
        out.extend_from_slice(&digits[1..ndigits]);
    }
    out.push(b'e');
    out.push(if k + ndigits as i32 - 1 < 0 {
        b'-'
    } else {
        b'+'
    });

    let mut e = exp;
    let mut cent = 0i32;
    if e > 99 {
        cent = e / 100;
        out.push((cent as u8) + b'0');
        e -= cent * 100;
    }
    if e > 9 {
        let dec = e / 10;
        out.push((dec as u8) + b'0');
        e -= dec * 10;
    } else if cent != 0 {
        out.push(b'0');
    }
    out.push(((e % 10) as u8) + b'0');
}

fn fpconv_dtoa_into(d: f64, out: &mut Vec<u8>) {
    let neg = d.to_bits() & FPCONV_SIGNMASK != 0;
    let mut digits = [0u8; 24];
    let mut k = 0i32;
    let ndigits = fpconv_grisu2(d, &mut digits, &mut k);
    if neg {
        out.push(b'-');
    }
    fpconv_emit_digits_into(&digits, ndigits, k, neg, out);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub frame: RespFrame,
    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowedCommandParseResult<'a> {
    pub frame: BorrowedCommandFrame<'a>,
    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorrowedCommandFrame<'a> {
    NullArray,
    Arguments(Vec<&'a [u8]>),
    Owned(RespFrame),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorrowedCommandArgsParseResult {
    pub kind: BorrowedCommandArgsKind,
    pub consumed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowedCommandArgsKind {
    NullArray,
    Arguments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserConfig {
    pub max_bulk_len: usize,
    pub max_array_len: usize,
    pub max_recursion_depth: usize,
    /// Opt-in RESP3 reply parsing. The default (`false`) keeps the
    /// fail-closed posture codified by
    /// `fr_p2c_002_u007_resp3_fail_closed_prefix_matrix`: any RESP3
    /// type prefix on untrusted input is rejected with
    /// `UnsupportedResp3Type`. Trusted callers (e.g. the live-oracle
    /// harness reading replies from the vendored redis-server after
    /// `HELLO 3`) flip this to `true` so RESP3 frames downgrade into
    /// RESP2-equivalent `RespFrame` shapes — map → flat Array of 2N
    /// field/value entries, set/push → Array, bool → Integer, double
    /// / big-number → BulkString of the ASCII form, null → BulkString
    /// None, verbatim → BulkString (prefix stripped), attribute →
    /// peeled before parsing the next real frame, blob-error → Error.
    /// (br-frankenredis-ozcx)
    pub allow_resp3: bool,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            max_bulk_len: 512 * 1024 * 1024, // 512 MiB default (Redis standard)
            max_array_len: 1024 * 1024,      // 1M elements
            max_recursion_depth: 128,
            allow_resp3: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespParseError {
    Incomplete,
    InvalidPrefix(u8),
    UnsupportedResp3Type(u8),
    InvalidInteger,
    InvalidBulkLength,
    InvalidMultibulkLength,
    InvalidUtf8,
    BulkLengthTooLarge,
    MultibulkLengthTooLarge,
    RecursionLimitExceeded,
    LineTooLong,
    /// (frankenredis-5qqv1) A command multibulk element was not a `$` bulk
    /// string. Carries the offending type byte for the upstream wording
    /// "expected '$', got 'X'".
    ExpectedBulk(u8),
    /// An inline request exceeded `PROTO_INLINE_MAX_SIZE` (64 KiB) before a
    /// terminating newline arrived. Upstream networking.c::processInlineBuffer
    /// (line 2146) replies "Protocol error: too big inline request" and closes
    /// the connection via setProtocolError.
    InlineRequestTooBig,
    /// An inline request had unbalanced quotes (sdssplitargs returned NULL).
    /// Upstream networking.c::processInlineBuffer (line 2161) replies
    /// "Protocol error: unbalanced quotes in request" and closes the connection
    /// via setProtocolError — like every other inline/multibulk protocol error.
    UnbalancedInlineQuotes,
    /// A multibulk `*<count>` header line exceeded the line cap before its
    /// terminator. Upstream networking.c::processMultibulkBuffer replies
    /// "Protocol error: too big mbulk count string". This is the count-line
    /// twin of the generic `LineTooLong`. (frankenredis-linetoolong-wording)
    TooBigMbulkCount,
    /// A bulk `$<len>` header line — or a multibulk element's header — exceeded
    /// the line cap before its terminator. Upstream replies "Protocol error:
    /// too big bulk count string". (frankenredis-linetoolong-wording)
    TooBigBulkCount,
}

impl Display for RespParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete frame"),
            Self::InvalidPrefix(ch) => write!(f, "invalid RESP type prefix: {}", char::from(*ch)),
            Self::UnsupportedResp3Type(ch) => {
                write!(f, "unsupported RESP3 type prefix: {}", char::from(*ch))
            }
            Self::InvalidInteger => write!(f, "invalid RESP integer"),
            Self::InvalidBulkLength => write!(f, "invalid bulk length"),
            Self::InvalidMultibulkLength => write!(f, "invalid multibulk length"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 payload"),
            // (frankenredis-w7xy8) Upstream networking.c emits the SAME message
            // for malformed and over-limit lengths ("invalid bulk length" /
            // "invalid multibulk length"), so a client sees identical wording
            // whether it sent `$abc` or `$<huge>`.
            Self::BulkLengthTooLarge => write!(f, "invalid bulk length"),
            Self::MultibulkLengthTooLarge => write!(f, "invalid multibulk length"),
            Self::RecursionLimitExceeded => write!(f, "nested array depth limit exceeded"),
            Self::LineTooLong => write!(f, "RESP line too long"),
            Self::ExpectedBulk(got) => write!(f, "expected '$', got '{}'", char::from(*got)),
            Self::InlineRequestTooBig => write!(f, "too big inline request"),
            Self::UnbalancedInlineQuotes => write!(f, "unbalanced quotes in request"),
            Self::TooBigMbulkCount => write!(f, "too big mbulk count string"),
            Self::TooBigBulkCount => write!(f, "too big bulk count string"),
        }
    }
}

/// Map a `LineTooLong` from a command-input header read to the context-specific
/// upstream wording ("too big mbulk/bulk count string"); pass any other error
/// through unchanged. (frankenredis-linetoolong-wording)
fn line_too_long_as(e: RespParseError, mapped: RespParseError) -> RespParseError {
    if matches!(e, RespParseError::LineTooLong) {
        mapped
    } else {
        e
    }
}

impl Error for RespParseError {}

pub fn parse_frame(input: &[u8]) -> Result<ParseResult, RespParseError> {
    parse_frame_with_config(input, &ParserConfig::default())
}

pub fn parse_frame_with_config(
    input: &[u8],
    config: &ParserConfig,
) -> Result<ParseResult, RespParseError> {
    let (frame, consumed) = parse_frame_internal(input, 0, 0, 0, config)?;
    Ok(ParseResult { frame, consumed })
}

/// Parse a CLIENT → SERVER **command** frame. Unlike [`parse_frame_with_config`]
/// (which accepts any RESP type for array elements — correct for parsing
/// *replies*), every element of a command multibulk must be a non-null bulk
/// string, matching upstream networking.c::processMultibulkBuffer. A non-`$`
/// element yields `ExpectedBulk(byte)` ("expected '$', got 'X'") and a `$-1`
/// null argument yields `InvalidBulkLength`. (frankenredis-5qqv1)
///
/// Non-multibulk input (e.g. an inline command, which never reaches here)
/// falls through to the generic parser.
pub fn parse_command_frame(
    input: &[u8],
    config: &ParserConfig,
) -> Result<ParseResult, RespParseError> {
    if input.first() != Some(&b'*') {
        return parse_frame_with_config(input, config);
    }
    let (line, mut cursor) =
        read_line(input, 1).map_err(|e| line_too_long_as(e, RespParseError::TooBigMbulkCount))?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    // (frankenredis-6dpyk) Upstream networking.c::processMultibulkBuffer consumes
    // ANY multibulk count <= 0 as a no-op (no command, no reply), not just the
    // canonical *-1 null array — `*-2`/`*-3` must NOT error. Surface every
    // negative count as the null-array frame the server already treats as a
    // no-op; `*0` (len == 0) still falls through to the empty-array path below.
    if len < 0 {
        return Ok(ParseResult {
            frame: RespFrame::Array(None),
            consumed: cursor,
        });
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        match input.get(cursor) {
            None => return Err(RespParseError::Incomplete),
            Some(&b'$') => {}
            Some(&other) => {
                // (frankenredis-mbulkdefer) Match upstream
                // networking.c::processMultibulkBuffer, which locates the
                // element's line terminator (strchr '\r') BEFORE checking the
                // type byte: a malformed element whose line hasn't fully
                // arrived must WAIT (Incomplete), not error early. read_line
                // yields Incomplete (no `\r\n` yet, within the line cap),
                // LineTooLong (over cap == upstream "too big bulk count
                // string"), or Ok (line complete -> the type byte is
                // definitively wrong -> ExpectedBulk).
                read_line(input, cursor)
                    .map_err(|e| line_too_long_as(e, RespParseError::TooBigBulkCount))?;
                return Err(RespParseError::ExpectedBulk(other));
            }
        }
        let (item, consumed) = parse_bulk(input, cursor + 1, config, false)?;
        // A `$-1` null bulk is a valid *reply* but never a command argument.
        if matches!(item, RespFrame::BulkString(None)) {
            return Err(RespParseError::InvalidBulkLength);
        }
        items.push(item);
        cursor = consumed;
    }
    Ok(ParseResult {
        frame: RespFrame::Array(Some(items)),
        consumed: cursor,
    })
}

/// Parse a CLIENT -> SERVER command frame while borrowing multibulk arguments
/// from `input`.
///
/// This mirrors [`parse_command_frame`] for validation and error behavior, but
/// the normal `*N\r\n$...\r\n` command path returns `&[u8]` argument slices
/// instead of allocating one `Vec<u8>` per bulk string. Non-multibulk input
/// falls back to the generic owned parser for the same reason
/// [`parse_command_frame`] does: callers may still be handling a non-command
/// RESP frame on a shared parsing path.
pub fn parse_command_frame_borrowed<'a>(
    input: &'a [u8],
    config: &ParserConfig,
) -> Result<BorrowedCommandParseResult<'a>, RespParseError> {
    if input.first() != Some(&b'*') {
        let parsed = parse_frame_with_config(input, config)?;
        return Ok(BorrowedCommandParseResult {
            frame: BorrowedCommandFrame::Owned(parsed.frame),
            consumed: parsed.consumed,
        });
    }
    let mut args = Vec::new();
    let parsed = parse_command_args_borrowed_into(input, config, &mut args)?;
    let frame = match parsed.kind {
        BorrowedCommandArgsKind::NullArray => BorrowedCommandFrame::NullArray,
        BorrowedCommandArgsKind::Arguments => BorrowedCommandFrame::Arguments(args),
    };
    Ok(BorrowedCommandParseResult {
        frame,
        consumed: parsed.consumed,
    })
}

/// Parse a strict RESP multibulk command into a caller-reused borrowed argv
/// buffer. This is the allocation-minimal primitive for future hot-path
/// runtime wiring; it is multibulk-only, so non-`*` input is rejected instead
/// of using the owned fallback in [`parse_command_frame_borrowed`].
pub fn parse_command_args_borrowed_into<'a>(
    input: &'a [u8],
    config: &ParserConfig,
    args: &mut Vec<&'a [u8]>,
) -> Result<BorrowedCommandArgsParseResult, RespParseError> {
    args.clear();
    match parse_command_args_borrowed_into_inner(input, config, args) {
        Ok(parsed) => Ok(parsed),
        Err(err) => {
            args.clear();
            Err(err)
        }
    }
}

fn parse_command_args_borrowed_into_inner<'a>(
    input: &'a [u8],
    config: &ParserConfig,
    args: &mut Vec<&'a [u8]>,
) -> Result<BorrowedCommandArgsParseResult, RespParseError> {
    match input.first() {
        Some(b'*') => {}
        Some(&other) => return Err(RespParseError::InvalidPrefix(other)),
        None => return Err(RespParseError::Incomplete),
    }
    let (line, mut cursor) =
        read_line(input, 1).map_err(|e| line_too_long_as(e, RespParseError::TooBigMbulkCount))?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    // (frankenredis-6dpyk) Any multibulk count <= 0 is a no-op upstream — `*-2`
    // must not error. Mirror parse_command_frame: every negative count becomes
    // the null-array no-op; `*0` falls through to the empty-args path.
    if len < 0 {
        return Ok(BorrowedCommandArgsParseResult {
            kind: BorrowedCommandArgsKind::NullArray,
            consumed: cursor,
        });
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let reserve = count.min(1024);
    if args.capacity() < reserve {
        args.reserve(reserve - args.capacity());
    }
    for _ in 0..count {
        match input.get(cursor) {
            None => return Err(RespParseError::Incomplete),
            Some(&b'$') => {}
            Some(&other) => {
                // (frankenredis-mbulkdefer) Mirror upstream
                // processMultibulkBuffer: find the element's line terminator
                // before checking the type byte, so a malformed element whose
                // line hasn't fully arrived WAITS (Incomplete) instead of
                // erroring early. See the owned-parser twin above.
                read_line(input, cursor)
                    .map_err(|e| line_too_long_as(e, RespParseError::TooBigBulkCount))?;
                return Err(RespParseError::ExpectedBulk(other));
            }
        }
        let (arg, consumed) = parse_bulk_slice(input, cursor + 1, config)?;
        let Some(arg) = arg else {
            return Err(RespParseError::InvalidBulkLength);
        };
        args.push(arg);
        cursor = consumed;
    }
    Ok(BorrowedCommandArgsParseResult {
        kind: BorrowedCommandArgsKind::Arguments,
        consumed: cursor,
    })
}

/// Maximum number of consecutive RESP3 attribute prefixes ('|...')
/// the parser will follow before returning RecursionLimitExceeded.
/// Independent of `max_recursion_depth` because the attribute branch
/// is depth-transparent (xfxtd) — without a separate cap an attacker
/// could send '|N\r\n+k\r\n+v\r\n' × M as untrusted input and grow
/// the Rust call stack linearly with M, regardless of the recursion-
/// depth budget. (frankenredis-oafun)
const RESP3_ATTRIBUTE_CHAIN_LIMIT: usize = 8;

fn parse_frame_internal(
    input: &[u8],
    start: usize,
    depth: usize,
    attr_chain_depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    if depth > config.max_recursion_depth {
        return Err(RespParseError::RecursionLimitExceeded);
    }
    if attr_chain_depth > RESP3_ATTRIBUTE_CHAIN_LIMIT {
        return Err(RespParseError::RecursionLimitExceeded);
    }
    let prefix = *input.get(start).ok_or(RespParseError::Incomplete)?;
    let next = start + 1;
    match prefix {
        b'+' => {
            let (line, consumed) = read_line(input, next)?;
            let raw = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            let text = sanitize_inline_body(raw);
            Ok((RespFrame::SimpleString(text), consumed))
        }
        b'-' => {
            let (line, consumed) = read_line(input, next)?;
            let raw = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            let text = sanitize_inline_body(raw);
            Ok((RespFrame::Error(text), consumed))
        }
        b':' => {
            let (line, consumed) = read_line(input, next)?;
            let n = parse_i64_strict(line)?;
            Ok((RespFrame::Integer(n), consumed))
        }
        b'$' => parse_bulk(input, next, config, true),
        b'*' => parse_array(input, next, depth, config),
        // RESP3 type prefixes. Without `config.allow_resp3`, hard-reject
        // every RESP3 prefix to preserve the fail-closed posture
        // codified by fr_p2c_002_u007_resp3_fail_closed_prefix_matrix.
        // With `allow_resp3`, downgrade each type into a RESP2-shaped
        // RespFrame so downstream RESP2-only code can consume the
        // reply. (br-frankenredis-ozcx)
        b'~' | b'%' | b'#' | b',' | b'_' | b'(' | b'=' | b'|' | b'>' | b'!'
            if !config.allow_resp3 =>
        {
            Err(RespParseError::UnsupportedResp3Type(prefix))
        }
        b'%' => parse_resp3_map(input, next, depth, config),
        b'~' | b'>' => parse_array(input, next, depth, config),
        b'#' => parse_resp3_bool(input, next),
        b',' => {
            // RESP3 double: ',<f64>\r\n'. The value must parse as a
            // double (incl. 'inf', '-inf', 'nan'). Empty / non-
            // numeric payloads are malformed — reject same way
            // ny5fu rejects '_payload\r\n'. (frankenredis-u1xg5)
            let (line, consumed) = read_line(input, next)?;
            let s = std::str::from_utf8(line).map_err(|_| RespParseError::InvalidUtf8)?;
            if s.is_empty() || s.parse::<f64>().is_err() {
                return Err(RespParseError::InvalidInteger);
            }
            Ok((RespFrame::BulkString(Some(s.as_bytes().to_vec())), consumed))
        }
        b'(' => {
            // RESP3 big number: '(<base-10-integer>\r\n'. Body must
            // be all decimal digits with optional leading +/- sign.
            // Empty / non-numeric payloads are malformed.
            // (frankenredis-u1xg5)
            let (line, consumed) = read_line(input, next)?;
            if line.is_empty() {
                return Err(RespParseError::InvalidInteger);
            }
            let digits_start = match line[0] {
                b'+' | b'-' => 1,
                _ => 0,
            };
            if digits_start == line.len()
                || line[digits_start..].iter().any(|b| !b.is_ascii_digit())
            {
                return Err(RespParseError::InvalidInteger);
            }
            let s = std::str::from_utf8(line)
                .map_err(|_| RespParseError::InvalidUtf8)?
                .to_string();
            Ok((RespFrame::BulkString(Some(s.into_bytes())), consumed))
        }
        b'_' => {
            let (line, consumed) = read_line(input, next)?;
            if !line.is_empty() {
                return Err(RespParseError::InvalidBulkLength);
            }
            Ok((RespFrame::BulkString(None), consumed))
        }
        b'=' => parse_resp3_verbatim(input, next, config),
        b'|' => {
            // Attribute: parse the attribute map and discard it,
            // then return the next real frame. Upstream uses this
            // to attach per-reply metadata that RESP2 clients don't
            // understand. The attribute is transparent for the
            // depth budget (xfxtd) — but chained attribute prefixes
            // ('|...|...|...frame') must still be capped so an
            // attacker can't grow the Rust call stack at the same
            // depth without bound. (frankenredis-oafun)
            let (_attr, consumed) = parse_resp3_map(input, next, depth, config)?;
            parse_frame_internal(input, consumed, depth, attr_chain_depth + 1, config)
        }
        b'!' => parse_resp3_blob_error(input, next, config),
        other => Err(RespParseError::InvalidPrefix(other)),
    }
}

fn parse_resp3_map(
    input: &[u8],
    start: usize,
    depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, mut cursor) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    if len < 0 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    let pair_count = count
        .checked_mul(2)
        .ok_or(RespParseError::MultibulkLengthTooLarge)?;
    if pair_count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(pair_count.min(1024));
    for _ in 0..pair_count {
        // Child frames reset attr_chain_depth: each k/v pair is its
        // own independent frame, not part of the outer attribute
        // chain. (frankenredis-oafun)
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, 0, config)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

fn parse_resp3_bool(input: &[u8], start: usize) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let flag = match line {
        b"t" => 1,
        b"f" => 0,
        _ => return Err(RespParseError::InvalidInteger),
    };
    Ok((RespFrame::Integer(flag), consumed))
}

fn parse_resp3_verbatim(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len < 0 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    // Verbatim string body is `<3-char-type>:<payload>`. The format
    // tag is mandatory even when the payload is empty.
    let body = &input[consumed..consumed + data_len];
    if body.len() < 4 || body[3] != b':' {
        return Err(RespParseError::InvalidBulkLength);
    }
    Ok((RespFrame::BulkString(Some(body[4..].to_vec())), end))
}

fn parse_resp3_blob_error(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len < 0 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    if input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n' {
        return Err(RespParseError::InvalidBulkLength);
    }
    let text = std::str::from_utf8(&input[consumed..consumed + data_len])
        .map(str::to_owned)
        .map_err(|_| RespParseError::InvalidUtf8)?;
    Ok((RespFrame::Error(text), end))
}

fn parse_bulk(
    input: &[u8],
    start: usize,
    config: &ParserConfig,
    validate_terminator: bool,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, consumed) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len == -1 {
        return Ok((RespFrame::BulkString(None), consumed));
    }
    if len < -1 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    // Command parsing does NOT validate the 2 bytes after the payload:
    // upstream `processMultibulkBuffer` advances `qb_pos += bulklen+2`
    // unconditionally, so a length-mismatched bulk is re-split into the next
    // command rather than rejected. Reply parsing keeps the strict check.
    // (frankenredis-v4cl4)
    if validate_terminator
        && (input[consumed + data_len] != b'\r' || input[consumed + data_len + 1] != b'\n')
    {
        return Err(RespParseError::InvalidBulkLength);
    }
    let bytes = input[consumed..consumed + data_len].to_vec();
    Ok((RespFrame::BulkString(Some(bytes)), end))
}

fn parse_bulk_slice<'a>(
    input: &'a [u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(Option<&'a [u8]>, usize), RespParseError> {
    parse_bulk_slice_impl::<true>(input, start, config)
}

/// `FAST == true` (production) fuses the header scan for the overwhelmingly common
/// `<nonzero-digit><digits...>\r\n` bulk length — every command argument — into a SINGLE pass that
/// accumulates the length AND locates the CRLF, instead of `read_line` (which scans for CRLF) then
/// `parse_i64_strict` (which re-scans the same digits). Any deviation (leading `0`, `-1`/negative,
/// non-digit, >18 digits, or a malformed/incomplete CRLF) FALLS THROUGH to the exact prior slow
/// path, so every reply, error, and Incomplete boundary is byte-identical. `FAST == false` forces
/// that slow path verbatim for the same-binary A/B in `benches/parse_bulk_slice_fastpath.rs`.
#[inline]
fn parse_bulk_slice_impl<'a, const FAST: bool>(
    input: &'a [u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(Option<&'a [u8]>, usize), RespParseError> {
    if FAST
        && let Some(&first) = input.get(start)
        && first.is_ascii_digit()
        && first != b'0'
    {
        let mut val = (first - b'0') as u64;
        let mut i = start + 1;
        // 18 digits keeps `val` far inside u64 and below any realistic proto-max-bulk-len; a
        // longer (or overflowing) length is rare and falls back for exact handling.
        let digit_limit = start + 18;
        loop {
            match input.get(i) {
                Some(&b) if b.is_ascii_digit() => {
                    if i >= digit_limit {
                        break; // too many digits — fall back
                    }
                    val = val * 10 + u64::from(b - b'0');
                    i += 1;
                }
                Some(&b'\r') if input.get(i + 1) == Some(&b'\n') => {
                    let consumed = i + 2;
                    let data_len = val as usize;
                    if data_len > config.max_bulk_len {
                        return Err(RespParseError::BulkLengthTooLarge);
                    }
                    let end = consumed
                        .checked_add(data_len)
                        .and_then(|idx| idx.checked_add(2))
                        .ok_or(RespParseError::Incomplete)?;
                    if input.len() < end {
                        return Err(RespParseError::Incomplete);
                    }
                    // Command path: do NOT validate the trailing 2 bytes. (frankenredis-v4cl4)
                    return Ok((Some(&input[consumed..consumed + data_len]), end));
                }
                _ => break, // non-digit / lone '\r' / incomplete CRLF — fall back
            }
        }
    }

    // Slow path — also the fast path's fallback; exact prior behavior.
    let (line, consumed) = read_line(input, start)
        .map_err(|e| line_too_long_as(e, RespParseError::TooBigBulkCount))?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidBulkLength)?;
    if len == -1 {
        return Ok((None, consumed));
    }
    if len < -1 {
        return Err(RespParseError::InvalidBulkLength);
    }
    let data_len = usize::try_from(len).map_err(|_| RespParseError::InvalidBulkLength)?;
    if data_len > config.max_bulk_len {
        return Err(RespParseError::BulkLengthTooLarge);
    }
    let end = consumed
        .checked_add(data_len)
        .and_then(|idx| idx.checked_add(2))
        .ok_or(RespParseError::Incomplete)?;
    if input.len() < end {
        return Err(RespParseError::Incomplete);
    }
    // Command path: do NOT validate the trailing 2 bytes (see `parse_bulk`) —
    // upstream advances past them unconditionally. (frankenredis-v4cl4)
    Ok((Some(&input[consumed..consumed + data_len]), end))
}

/// Bench hook for the same-binary A/B in `benches/parse_bulk_slice_fastpath.rs`.
/// `FAST = false` forces the prior `read_line` + `parse_i64_strict` two-pass path. Not production.
#[doc(hidden)]
#[inline(never)]
pub fn bench_parse_bulk_slice<'a, const FAST: bool>(
    input: &'a [u8],
    start: usize,
    config: &ParserConfig,
) -> Result<(Option<&'a [u8]>, usize), RespParseError> {
    parse_bulk_slice_impl::<FAST>(input, start, config)
}

fn parse_array(
    input: &[u8],
    start: usize,
    depth: usize,
    config: &ParserConfig,
) -> Result<(RespFrame, usize), RespParseError> {
    let (line, mut cursor) = read_line(input, start)?;
    let len = parse_i64_strict(line).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if len == -1 {
        return Ok((RespFrame::Array(None), cursor));
    }
    if len < -1 {
        return Err(RespParseError::InvalidMultibulkLength);
    }
    let count = usize::try_from(len).map_err(|_| RespParseError::InvalidMultibulkLength)?;
    if count > config.max_array_len {
        return Err(RespParseError::MultibulkLengthTooLarge);
    }
    let mut items = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        // Child frames reset attr_chain_depth — each array element
        // is its own independent frame. (frankenredis-oafun)
        let (item, consumed) = parse_frame_internal(input, cursor, depth + 1, 0, config)?;
        items.push(item);
        cursor = consumed;
    }
    Ok((RespFrame::Array(Some(items)), cursor))
}

const MAX_LINE_LENGTH: usize = 64 * 1024; // 64 KiB

fn parse_i64_strict(input: &[u8]) -> Result<i64, RespParseError> {
    parse_i64_strict_impl::<true, true, true, true>(input)
}

/// Bench hook for the same-binary A/B in `benches/parse_i64_fastpath.rs`.
///
/// `FAST = false` forces the pre-fast-path guarded loop (per-digit u64-overflow checks on every
/// input). `ONE_DIGIT = false` forces the prior general-parser path for one-digit inputs,
/// `TWO_DIGIT = false` retains the exact parser that production used before the fixed-width
/// positive two-digit shortcut, and `THREE_DIGIT = false` retains the parser that production used
/// before the fixed-width positive three-digit shortcut. No reference configuration is on a
/// production path.
#[doc(hidden)]
#[inline(never)]
pub fn bench_parse_i64_strict<
    const FAST: bool,
    const ONE_DIGIT: bool,
    const TWO_DIGIT: bool,
    const THREE_DIGIT: bool,
>(
    input: &[u8],
) -> Result<i64, RespParseError> {
    parse_i64_strict_impl::<FAST, ONE_DIGIT, TWO_DIGIT, THREE_DIGIT>(input)
}

fn parse_i64_strict_impl<
    const FAST: bool,
    const ONE_DIGIT: bool,
    const TWO_DIGIT: bool,
    const THREE_DIGIT: bool,
>(
    input: &[u8],
) -> Result<i64, RespParseError> {
    let slen = input.len();
    if slen == 0 || slen > 20 {
        return Err(RespParseError::InvalidInteger);
    }
    // The overwhelmingly common RESP length/count headers are one digit (`$3`, `*2`, ...).
    // The previous `slen == 1 && input[0] == b'0'` check already paid the length branch, but only
    // zero returned there; 1..=9 continued through sign detection, range checks, accumulator setup,
    // and final i64 bounds. Return every valid one-digit integer directly. Multi-digit inputs take
    // the same not-taken length branch as before, and the reference arm below retains the literal
    // old path for the in-binary A/B.
    if ONE_DIGIT && slen == 1 {
        let digit = input[0].wrapping_sub(b'0');
        return if digit <= 9 {
            Ok(i64::from(digit))
        } else {
            Err(RespParseError::InvalidInteger)
        };
    }
    // Bulk lengths and array counts commonly sit in 10..=99. Once the two bytes are known ASCII
    // digits with a nonzero tens place, their value is fixed and cannot reach any sign or range
    // edge. Return it before the general sign/accumulator/final-bounds path. Invalid, leading-zero,
    // and signed two-byte inputs deliberately fall through to the literal prior parser below.
    if TWO_DIGIT && slen == 2 {
        let tens = input[0].wrapping_sub(b'0');
        let ones = input[1].wrapping_sub(b'0');
        if (1..=9).contains(&tens) && ones <= 9 {
            return Ok(i64::from(tens * 10 + ones));
        }
    }
    // Medium bulk lengths (`$100`..`$999`) — values a few hundred bytes long — are the common
    // three-digit RESP header. Three ASCII digits with a nonzero hundreds place have a fixed value
    // (max 999) that cannot reach any sign or i64 range edge, so return it before the general
    // sign/accumulator/final-bounds path. Leading-zero, signed, and invalid three-byte inputs
    // deliberately fall through to the literal prior parser below.
    if THREE_DIGIT && slen == 3 {
        let hundreds = input[0].wrapping_sub(b'0');
        let tens = input[1].wrapping_sub(b'0');
        let ones = input[2].wrapping_sub(b'0');
        if (1..=9).contains(&hundreds) && tens <= 9 && ones <= 9 {
            return Ok(i64::from(hundreds) * 100 + i64::from(tens) * 10 + i64::from(ones));
        }
    }
    if slen == 1 && input[0] == b'0' {
        return Ok(0);
    }

    let mut p = 0;
    let negative = input[0] == b'-';
    if negative {
        p += 1;
        if p == slen {
            return Err(RespParseError::InvalidInteger);
        }
    }

    if input[p] >= b'1' && input[p] <= b'9' {
        let mut v: u64 = (input[p] - b'0') as u64;
        p += 1;
        // (perf) A value with <= 19 decimal digits can never overflow u64 (max 19-digit
        // ~1e19 < u64::MAX ~1.84e19), so the per-digit overflow guards below are dead code on
        // that path — the common bulk-length / multibulk-count case. Take an unchecked
        // accumulation loop then; only a 20-digit input (positive only — a negative caps at 19
        // digits since `slen > 20` was already rejected) keeps the guarded loop. Byte-identical:
        // the parsed `v` is the same (no overflow was possible) and the final i64-range check
        // below is unchanged, so every result and error matches the guarded path.
        // Gated on `p < slen` so a SINGLE-digit length (the hot `$3` / `*3` header) skips the
        // digit-count computation + branch entirely and stays byte-for-byte the pre-change path
        // (its loop ran zero times anyway) — Pareto-safe, faster only where there are 2+ digits.
        if p < slen {
            let digit_count = slen - usize::from(negative);
            if FAST && digit_count <= 19 {
                while p < slen {
                    let b = input[p];
                    if !b.is_ascii_digit() {
                        return Err(RespParseError::InvalidInteger);
                    }
                    v = v * 10 + (b - b'0') as u64;
                    p += 1;
                }
            } else {
                while p < slen {
                    let b = input[p];
                    if b.is_ascii_digit() {
                        if v > (u64::MAX / 10) {
                            return Err(RespParseError::InvalidInteger);
                        }
                        v *= 10;
                        let digit = (b - b'0') as u64;
                        if v > (u64::MAX - digit) {
                            return Err(RespParseError::InvalidInteger);
                        }
                        v += digit;
                        p += 1;
                    } else {
                        return Err(RespParseError::InvalidInteger);
                    }
                }
            }
        }

        if negative {
            let limit = (i64::MIN as u64).wrapping_neg();
            if v > limit {
                return Err(RespParseError::InvalidInteger);
            }
            return Ok(v.wrapping_neg() as i64);
        } else {
            if v > i64::MAX as u64 {
                return Err(RespParseError::InvalidInteger);
            }
            return Ok(v as i64);
        }
    }

    Err(RespParseError::InvalidInteger)
}

fn read_line(input: &[u8], start: usize) -> Result<(&[u8], usize), RespParseError> {
    if start >= input.len() {
        return Err(RespParseError::Incomplete);
    }
    let max_line_end = start.saturating_add(MAX_LINE_LENGTH);
    let mut i = start;
    while i + 1 < input.len() {
        if input[i] == b'\r' && input[i + 1] == b'\n' {
            return Ok((&input[start..i], i + 2));
        }
        i += 1;
        if i > max_line_end {
            return Err(RespParseError::LineTooLong);
        }
    }
    Err(RespParseError::Incomplete)
}

#[cfg(test)]
mod d2string_edge_cases {
    use super::{
        ZsetScoreListpackEntry, format_redis_double, push_redis_double_ascii,
        zset_score_listpack_entry,
    };

    /// `(f64 bit pattern, exact text)` — captured from **vendored redis 7.2.4** by running
    /// `ZADD z <repr> m; ZSCORE z m` against a live server (2026-07-10). All 51 rows also had
    /// byte-identical `DUMP` payloads between fr and the oracle.
    ///
    /// Keyed by bit pattern, not by literal, so subnormals survive the source round trip.
    ///
    /// These pin `d2string` (`util.c`) at the boundaries that actually bite:
    ///   * the exact-integer `ll2string` window is `±(LLONG_MAX/2)` == 2^62, NOT 2^52 —
    ///     `1e16`/`1e18`/`4e18`/`2^62` print as plain decimals, `5e18` prints as `"5e+18"`;
    ///   * grisu2's plain-vs-scientific switch: `1e-5` -> `"0.00001"` but `1e-7` -> `"1e-7"`,
    ///     and `123456789.12345679` -> `"1.2345678912345679e+8"` (scientific at ~1.2e8);
    ///   * 17-significant-digit round trips (`0.1+0.2`, `1.2345678901234567`);
    ///   * subnormals down to `5e-324`, and the normal/subnormal boundary;
    ///   * `inf` / `-inf`.
    ///
    /// Regressing any row means a Redis client reading our RDB or `ZSCORE` sees different
    /// score TEXT than upstream would emit.
    const VENDORED: &[(u64, &str)] = &[
        (0x0000000000000000, "0"),
        (0x3FF0000000000000, "1"),
        (0xBFF0000000000000, "-1"),
        (0x405FC00000000000, "127"),
        (0x4060000000000000, "128"),
        (0x40B0000000000000, "4096"),
        (0x40E0000000000000, "32768"),
        (0x41E0000000000000, "2147483648"),
        (0x4330000000000000, "4503599627370496"),  // 2^52
        (0x4330000000000001, "4503599627370497"),  // 2^52 + 1
        (0x4340000000000000, "9007199254740992"),  // 2^53
        (0x430C6BF526340000, "1000000000000000"),  // 1e15
        (0x4341C37937E08000, "10000000000000000"), // 1e16 — plain, NOT "1e+16"
        (0x4376345785D8A000, "100000000000000000"),
        (0x43ABC16D674EC800, "1000000000000000000"), // 1e18
        (0x43BBC16D674EC800, "2000000000000000000"),
        (0x43CBC16D674EC800, "4000000000000000000"),
        (0x43D0000000000000, "4611686018427387904"), // 2^62 == double2ll's bound
        (0x43D0000000000001, "4611686018427389000"), // just above the bound, still plain
        (0x43D8000000000000, "6917529027641082000"), // grisu2 counterexample: plain + int-encodes
        (0x43E0000000000000, "9223372036854776000"), // 2^63: plain, but overflows i64
        (0x43D158E460913D00, "5e+18"),               // first round value that goes scientific
        (0x43D8493FBA64EF00, "7e+18"),
        (0x43E158E460913D00, "1e+19"),
        (0x4415AF1D78B58C40, "1e+20"),
        (0x444B1AE4D6E2EF50, "1e+21"),
        (0x3FB999999999999A, "0.1"),
        (0x3FC999999999999A, "0.2"),
        (0x3FD3333333333333, "0.3"),
        (0x3FD3333333333334, "0.30000000000000004"), // 0.1 + 0.2, 17 sig digits
        (0x4004000000000000, "2.5"),
        (0xC004000000000000, "-2.5"),
        (0x40091EB851EB851F, "3.14"),
        (0x3FD5555555555555, "0.3333333333333333"),
        (0x3FF3C0CA428C59FB, "1.2345678901234567"),
        (0x419D6F34547E6B75, "1.2345678912345679e+8"), // scientific at ~1.2e8
        (0x3EE4F8B588E368F1, "0.00001"),               // 1e-5 stays fixed
        (0x3EB0C6F7A0B5ED8D, "0.000001"),              // 1e-6 stays fixed
        (0x3E7AD7F29ABCAF48, "1e-7"),                  // 1e-7 flips to scientific
        (0x3DDB7CDFD9D7BDBB, "1e-10"),
        (0x2B2BFF2EE48E0530, "1e-100"),
        (0x0010000000000000, "2.2250738585072014e-308"), // smallest normal
        (0x000FFFFFFFFFFFFF, "2.225073858507201e-308"),  // largest subnormal
        (0x0000000000000001, "5e-324"),                  // smallest subnormal
        (0x8000000000000001, "-5e-324"),
        (0x7FEFFFFFFFFFFFFF, "1.7976931348623157e+308"), // f64::MAX
        (0x7E41EB2D66005835, "1.5e+300"),
        (0xFE41EB2D66005835, "-1.5e+300"),
        (0x7FF0000000000000, "inf"),
        (0xFFF0000000000000, "-inf"),
    ];

    #[test]
    fn format_redis_double_matches_vendored_d2string() {
        for (bits, want) in VENDORED {
            let v = f64::from_bits(*bits);
            assert_eq!(
                format_redis_double(v),
                *want,
                "d2string(bits 0x{bits:016X}) diverged from vendored redis 7.2.4"
            );
            let mut out = Vec::new();
            push_redis_double_ascii(&mut out, v);
            assert_eq!(out, want.as_bytes(), "push_redis_double_ascii disagreed");
        }
    }

    /// `-0.0` and `nan` never reach a stored score (`ZADD k -0` normalizes to `+0`; `ZADD k nan`
    /// is rejected by both engines), so they cannot be captured from `ZSCORE`. Pin them from
    /// `util.c::d2string`'s own special cases instead.
    #[test]
    fn format_redis_double_pins_unreachable_special_cases() {
        assert_eq!(format_redis_double(-0.0), "-0");
        assert_eq!(format_redis_double(0.0), "0");
        assert_eq!(format_redis_double(f64::NAN), "nan");
        assert_eq!(format_redis_double(-f64::NAN), "nan");
    }

    /// Every finite row must survive `text -> f64` unchanged: `d2string` is a shortest
    /// round-trip representation, so re-parsing must land on the identical bit pattern.
    #[test]
    fn vendored_text_round_trips_to_the_same_bits() {
        for (bits, text) in VENDORED {
            let v = f64::from_bits(*bits);
            if !v.is_finite() {
                continue;
            }
            let back: f64 = text.parse().expect("d2string output must parse as f64");
            assert_eq!(
                back.to_bits(),
                *bits,
                "{text} did not round-trip (bits 0x{bits:016X})"
            );
        }
    }

    /// `lpStringToInt64`'s canonical-decimal rule, as `parse_listpack_integer` implements it in
    /// fr-store / fr-persist. Duplicated here so this test needs no dependency on those crates.
    fn canonical_i64(text: &str) -> Option<i64> {
        let b = text.as_bytes();
        if b.is_empty() || b.len() >= 21 {
            return None;
        }
        let digits = if b[0] == b'-' { &b[1..] } else { b };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        if digits[0] == b'0' && digits.len() > 1 {
            return None;
        }
        if b[0] == b'-' && digits == b"0" {
            return None;
        }
        text.parse::<i64>().ok()
    }

    /// The contract that makes `zset_score_listpack_entry` safe, checked against the SAME text
    /// the vendored server emits: `Str` may skip the re-parse only if the render truly is not a
    /// canonical i64, and `Int(n)` must equal what the re-parse would have produced.
    #[test]
    fn classifier_agrees_with_vendored_render() {
        for (bits, text) in VENDORED {
            let v = f64::from_bits(*bits);
            match zset_score_listpack_entry(v) {
                ZsetScoreListpackEntry::Int(n) => assert_eq!(
                    canonical_i64(text),
                    Some(n),
                    "{text}: classified Int({n}) but the render says otherwise"
                ),
                ZsetScoreListpackEntry::Str => assert!(
                    canonical_i64(text).is_none(),
                    "{text}: classified Str, but its render re-parses as an integer — \
                     skipping the re-parse would flip the listpack entry encoding"
                ),
                ZsetScoreListpackEntry::Reparse => assert!(
                    v.is_finite() && v.fract() == 0.0 && v.abs() > (i64::MAX / 2) as f64,
                    "{text}: Reparse is only for integral doubles outside double2ll's window"
                ),
            }
        }
        // -0.0 is the one Str case not reachable through ZADD.
        assert!(matches!(
            zset_score_listpack_entry(-0.0),
            ZsetScoreListpackEntry::Str
        ));
        assert!(matches!(
            zset_score_listpack_entry(f64::NAN),
            ZsetScoreListpackEntry::Str
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BorrowedCommandArgsKind, BorrowedCommandFrame, MAX_LINE_LENGTH, ParserConfig, RespFrame,
        RespParseError, bench_encode_integer, bench_parse_bulk_slice, bench_push_len_header,
        decimal_u64_len, decimal_usize_len, encode_aggregate_header, encode_bulk_string_slice,
        encode_map_header, encode_redis_double, format_redis_double,
        parse_command_args_borrowed_into, parse_command_frame, parse_command_frame_borrowed,
        parse_frame, parse_frame_with_config, push_i64, push_redis_double_ascii, push_usize,
    };

    // The fused single-pass bulk-length fast path must be byte-identical to the prior
    // read_line + parse_i64_strict two-pass path for the full result — including every error and
    // Incomplete boundary — across hand-picked edge cases and a small exhaustive enumeration over
    // {digits, sign, CR, LF, junk} at multiple lengths.
    #[test]
    fn parse_bulk_slice_fast_matches_slow() {
        let config = ParserConfig::default();
        let mut cases: Vec<Vec<u8>> = vec![
            b"3\r\nfoo\r\n".to_vec(),
            b"0\r\n\r\n".to_vec(),
            b"5\r\nhello\r\n".to_vec(),
            b"11\r\nhelloworldx\r\n".to_vec(),
            b"128\r\n".to_vec(),
            b"-1\r\n".to_vec(),
            b"-2\r\n".to_vec(),
            b"\r\n".to_vec(),
            b"-\r\n".to_vec(),
            b"+3\r\n".to_vec(),
            b" 3\r\n".to_vec(),
            b"07\r\n........\r\n".to_vec(),
            b"007\r\n".to_vec(),
            b"1a\r\n".to_vec(),
            b"3\rX\r\n".to_vec(),
            b"3".to_vec(),
            b"3\r".to_vec(),
            b"3\r\n".to_vec(),
            b"3\r\nfo".to_vec(),
            b"123".to_vec(),
            b"".to_vec(),
            b"1234567890123456789\r\n".to_vec(),
            b"99999999999999999999\r\n".to_vec(),
        ];
        let alphabet = *b"0123\r\n-";
        for len in 0..=5usize {
            for mut idx in 0..7usize.pow(len as u32) {
                let mut buf = Vec::with_capacity(len);
                for _ in 0..len {
                    buf.push(alphabet[idx % 7]);
                    idx /= 7;
                }
                cases.push(buf);
            }
        }
        for buf in &cases {
            let fast = bench_parse_bulk_slice::<true>(buf, 0, &config);
            let slow = bench_parse_bulk_slice::<false>(buf, 0, &config);
            assert_eq!(fast, slow, "differ for {:?}", String::from_utf8_lossy(buf));
        }
    }

    // The fused single-buffer length header (`<prefix><n>\r\n` in one extend_from_slice) must be
    // byte-identical to the prior `extend(prefix) + push_usize + extend("\r\n")` shape across every
    // RESP prefix and a dense scan of small lengths plus the digit-width boundaries and u64 edge.
    // Also pins the three borrow-encode helpers that now front the reply path.
    #[test]
    fn fused_len_header_matches_three_call_and_helpers() {
        let mut sample: Vec<u64> = vec![
            0,
            9,
            10,
            99,
            100,
            999,
            1000,
            65_535,
            1_000_000,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ];
        for n in 0_u64..=5000 {
            sample.push(n);
        }
        for &prefix in &[b'$', b'*', b'~', b'%', b'>', b'|', b'='] {
            for &n in &sample {
                let mut fused = Vec::new();
                let mut old = Vec::new();
                bench_push_len_header::<true>(&mut fused, prefix, n);
                bench_push_len_header::<false>(&mut old, prefix, n);
                assert_eq!(fused, old, "fused vs three-call differ for {prefix:?} n={n}");
                assert_eq!(fused[0], prefix);
                assert_eq!(&fused[fused.len() - 2..], b"\r\n");
            }
        }
        // Non-empty destination appends, never overwrites.
        let mut buf = b"X".to_vec();
        bench_push_len_header::<true>(&mut buf, b'$', 128);
        assert_eq!(buf, b"X$128\r\n");

        // The borrow-encode helpers stay byte-identical to their documented wire form.
        let mut b = Vec::new();
        encode_bulk_string_slice(Some(b"hello"), false, &mut b);
        assert_eq!(b, b"$5\r\nhello\r\n");
        let mut a = Vec::new();
        encode_aggregate_header(3, false, &mut a);
        assert_eq!(a, b"*3\r\n");
        let mut s = Vec::new();
        encode_aggregate_header(2, true, &mut s);
        assert_eq!(s, b"~2\r\n");
        let mut m3 = Vec::new();
        encode_map_header(2, true, &mut m3);
        assert_eq!(m3, b"%2\r\n");
        let mut m2 = Vec::new();
        encode_map_header(2, false, &mut m2);
        assert_eq!(m2, b"*4\r\n");
    }

    // The fused single-buffer integer reply (`:<n>\r\n` in one extend_from_slice) must be
    // byte-identical to both the prior three-extend path and the canonical RespFrame encoder,
    // for every boundary plus a dense scan around zero and the ±small ranges that dominate real
    // counter/length replies.
    #[test]
    fn fused_integer_reply_matches_three_extend_and_frame() {
        let mut sample: Vec<i64> = vec![
            i64::MIN,
            i64::MIN + 1,
            i64::MAX,
            i64::MAX - 1,
            -1000000000000,
            -999,
            -100,
            -99,
            -10,
            0,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
        ];
        for n in -5000_i64..=5000 {
            sample.push(n);
        }
        for &n in &sample {
            let mut fused = Vec::new();
            let mut old = Vec::new();
            let mut frame = Vec::new();
            bench_encode_integer::<true>(n, &mut fused);
            bench_encode_integer::<false>(n, &mut old);
            RespFrame::Integer(n).encode_into(&mut frame);
            assert_eq!(fused, old, "fused vs three-extend differ for n={n}");
            assert_eq!(fused, frame, "fused vs frame differ for n={n}");
            // Sanity: the bytes are exactly `:<decimal>\r\n`.
            assert_eq!(fused[0], b':');
            assert_eq!(&fused[fused.len() - 2..], b"\r\n");
        }
        // Non-empty destination: the reply appends, never overwrites.
        let mut buf = b"PRE".to_vec();
        bench_encode_integer::<true>(42, &mut buf);
        assert_eq!(buf, b"PRE:42\r\n");
    }

    // (frankenredis-e4fu8) Lock the branchless ilog10 digit-count against the original
    #[test]
    fn parse_i64_strict_fast_path_matches_guarded_ref() {
        // Reference: the pre-fast-path parser that runs the per-digit u64-overflow guards on EVERY
        // input. Production `parse_i64_strict`'s <=19-digit fast path skips only guards that were
        // provably dead, so it must return the byte-identical `Result` for every input.
        fn ref_guarded(input: &[u8]) -> Result<i64, RespParseError> {
            let slen = input.len();
            if slen == 0 || slen > 20 {
                return Err(RespParseError::InvalidInteger);
            }
            if slen == 1 && input[0] == b'0' {
                return Ok(0);
            }
            let mut p = 0;
            let negative = input[0] == b'-';
            if negative {
                p += 1;
                if p == slen {
                    return Err(RespParseError::InvalidInteger);
                }
            }
            if input[p] >= b'1' && input[p] <= b'9' {
                let mut v: u64 = (input[p] - b'0') as u64;
                p += 1;
                while p < slen {
                    let b = input[p];
                    if b.is_ascii_digit() {
                        if v > (u64::MAX / 10) {
                            return Err(RespParseError::InvalidInteger);
                        }
                        v *= 10;
                        let digit = (b - b'0') as u64;
                        if v > (u64::MAX - digit) {
                            return Err(RespParseError::InvalidInteger);
                        }
                        v += digit;
                        p += 1;
                    } else {
                        return Err(RespParseError::InvalidInteger);
                    }
                }
                if negative {
                    let limit = (i64::MIN as u64).wrapping_neg();
                    if v > limit {
                        return Err(RespParseError::InvalidInteger);
                    }
                    return Ok(v.wrapping_neg() as i64);
                }
                if v > i64::MAX as u64 {
                    return Err(RespParseError::InvalidInteger);
                }
                return Ok(v as i64);
            }
            Err(RespParseError::InvalidInteger)
        }

        // The one-digit production shortcut must match the literal pre-shortcut path for every
        // possible input byte, not only valid ASCII digits. This pins invalid signs, high bytes,
        // and punctuation to the exact same error.
        for byte in 0_u8..=u8::MAX {
            let input = [byte];
            assert_eq!(
                super::parse_i64_strict(&input),
                super::parse_i64_strict_impl::<true, false, false, false>(&input),
                "one-byte input={byte:#04x}"
            );
        }

        // The positive two-digit return must match the exact immediately-prior parser for all
        // 65,536 possible byte pairs, including signs, leading zeros, punctuation, and high bytes.
        for first in 0_u8..=u8::MAX {
            for second in 0_u8..=u8::MAX {
                let input = [first, second];
                assert_eq!(
                    super::parse_i64_strict(&input),
                    super::parse_i64_strict_impl::<true, true, false, false>(&input),
                    "two-byte input=[{first:#04x}, {second:#04x}]"
                );
            }
        }

        // The new positive three-digit return must match the exact immediately-prior parser (with
        // the two-digit shortcut still on) for all 16,777,216 possible byte triples, including
        // signs, leading zeros, punctuation, and high bytes.
        for first in 0_u8..=u8::MAX {
            for second in 0_u8..=u8::MAX {
                for third in 0_u8..=u8::MAX {
                    let input = [first, second, third];
                    assert_eq!(
                        super::parse_i64_strict(&input),
                        super::parse_i64_strict_impl::<true, true, true, false>(&input),
                        "three-byte input=[{first:#04x}, {second:#04x}, {third:#04x}]"
                    );
                }
            }
        }

        // Exhaustive over {digits, sign, non-digit} up to length 5 (leading zeros, signs, junk).
        let alphabet = *b"0129-x";
        for len in 0..=5usize {
            for mut idx in 0..6usize.pow(len as u32) {
                let mut buf = Vec::with_capacity(len);
                for _ in 0..len {
                    buf.push(alphabet[idx % 6]);
                    idx /= 6;
                }
                assert_eq!(
                    super::parse_i64_strict(&buf),
                    ref_guarded(&buf),
                    "input={:?}",
                    String::from_utf8_lossy(&buf)
                );
            }
        }
        // 18-20-digit + i64/u64-boundary strings (the fast/slow-path seam and overflow edges).
        let boundaries: &[&[u8]] = &[
            b"9223372036854775807",
            b"9223372036854775808",
            b"-9223372036854775808",
            b"-9223372036854775809",
            b"18446744073709551615",
            b"18446744073709551616",
            b"99999999999999999999",
            b"9999999999999999999",
            b"1000000000000000000",
            b"-1000000000000000000",
            b"12345678901234567890",
        ];
        for b in boundaries {
            assert_eq!(
                super::parse_i64_strict(b),
                ref_guarded(b),
                "boundary={:?}",
                String::from_utf8_lossy(b)
            );
        }
    }

    // div-by-10 reference for every input that crosses a digit boundary, plus extremes.
    #[test]
    fn decimal_len_matches_div_loop_reference() {
        fn reference(mut n: u64) -> usize {
            let mut len = 1;
            while n >= 10 {
                n /= 10;
                len += 1;
            }
            len
        }
        let mut probes: Vec<u64> = vec![0, 1, 9, 10, 11, 99, 100, 101, u64::MAX, u64::MAX - 1];
        // every power-of-ten boundary and its neighbours
        let mut p: u64 = 1;
        loop {
            probes.push(p.saturating_sub(1));
            probes.push(p);
            probes.push(p.saturating_add(1));
            match p.checked_mul(10) {
                Some(next) => p = next,
                None => break,
            }
        }
        for &n in &probes {
            assert_eq!(decimal_u64_len(n), reference(n), "u64 digit count for {n}");
            if let Ok(u) = usize::try_from(n) {
                assert_eq!(
                    decimal_usize_len(u),
                    reference(n),
                    "usize digit count for {u}"
                );
            }
        }
    }

    // (frankenredis-432l0) Golden contract for how `parse_frame` NORMALIZES the
    // RESP3 reply wire types into the RESP2-shaped `RespFrame` the rest of fr
    // consumes. This is a reply-NORMALIZER, not a symmetric codec: the RESP3
    // scalar types collapse to their RESP2-equivalent carrier, and the
    // aggregate-ish types fold per upstream client semantics. Pinning each one
    // here guards the HELLO-3 reply path (frankenredis-ozcx) against regressions
    // that the request-side fuzzers never exercise. Each case is a COMPLETE
    // frame, so `consumed` must equal the encoded length.
    #[test]
    fn resp3_reply_parse_normalization_golden_432l0() {
        let cases: &[(&str, &[u8], RespFrame)] = &[
            // Double (`,`) -> BulkString carrying the numeric string verbatim.
            (
                "double",
                b",3.14\r\n",
                RespFrame::BulkString(Some(b"3.14".to_vec())),
            ),
            (
                "double_neg",
                b",-1.5\r\n",
                RespFrame::BulkString(Some(b"-1.5".to_vec())),
            ),
            (
                "double_inf",
                b",inf\r\n",
                RespFrame::BulkString(Some(b"inf".to_vec())),
            ),
            // Big number (`(`) -> BulkString carrying the base-10 integer string.
            (
                "bignumber",
                b"(12345678901234567890\r\n",
                RespFrame::BulkString(Some(b"12345678901234567890".to_vec())),
            ),
            (
                "bignumber_neg",
                b"(-31337\r\n",
                RespFrame::BulkString(Some(b"-31337".to_vec())),
            ),
            // Boolean (`#`) -> Integer 1/0 (upstream addReplyBool under RESP2).
            ("bool_true", b"#t\r\n", RespFrame::Integer(1)),
            ("bool_false", b"#f\r\n", RespFrame::Integer(0)),
            // Null (`_`) -> the canonical null bulk string.
            ("null", b"_\r\n", RespFrame::BulkString(None)),
            // Verbatim (`=`) -> BulkString of the payload AFTER the 4-char
            // `<3-type>:` prefix is stripped.
            (
                "verbatim_txt",
                b"=15\r\ntxt:Some string\r\n",
                RespFrame::BulkString(Some(b"Some string".to_vec())),
            ),
            // Map (`%`) -> flat Array of 2N field/value frames (recursively
            // normalized), matching upstream RESP3->RESP2 map downgrade.
            (
                "map_flattened",
                b"%2\r\n+a\r\n:1\r\n$1\r\nb\r\n:2\r\n",
                RespFrame::Array(Some(vec![
                    RespFrame::SimpleString("a".to_string()),
                    RespFrame::Integer(1),
                    RespFrame::BulkString(Some(b"b".to_vec())),
                    RespFrame::Integer(2),
                ])),
            ),
            // Set (`~`) -> folds to Array (fr has no distinct Set reply carrier).
            (
                "set_as_array",
                b"~2\r\n:1\r\n:2\r\n",
                RespFrame::Array(Some(vec![RespFrame::Integer(1), RespFrame::Integer(2)])),
            ),
            // Push (`>`) -> folds to Array.
            (
                "push_as_array",
                b">2\r\n+pubsub\r\n+msg\r\n",
                RespFrame::Array(Some(vec![
                    RespFrame::SimpleString("pubsub".to_string()),
                    RespFrame::SimpleString("msg".to_string()),
                ])),
            ),
            // Attribute (`|`) -> the metadata map is parsed and DISCARDED; the
            // following real frame is returned, and `consumed` spans both.
            (
                "attribute_discarded",
                b"|1\r\n+k\r\n+v\r\n:42\r\n",
                RespFrame::Integer(42),
            ),
            // Blob error (`!`) -> Error carrying the body text.
            (
                "blob_error",
                b"!21\r\nSYNTAX invalid syntax\r\n",
                RespFrame::Error("SYNTAX invalid syntax".to_string()),
            ),
        ];
        // RESP3 reply parsing is opt-in (the default fail-closed config rejects
        // these prefixes); trusted reply readers flip `allow_resp3`.
        let config = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        for (name, wire, expected) in cases {
            let parsed = parse_frame_with_config(wire, &config)
                .expect("RESP3 golden reply case must parse under allow_resp3");
            assert_eq!(&parsed.frame, expected, "{name}: normalized frame mismatch");
            assert_eq!(
                parsed.consumed,
                wire.len(),
                "{name}: must consume the whole frame"
            );
        }
    }

    // (frankenredis-itoa2) Two-digit-LUT integer formatter: isomorphism vs the
    // std `to_string()` reference (the ground truth) over exhaustive small
    // values, every power-of-ten / carry boundary, and i64 extremes; plus an
    // honest Score microbench of the old digit-at-a-time div loop vs the new
    // two-digit-per-step formatter on a realistic reply-integer mix.
    #[test]
    fn push_int_two_digit_lut_isomorphic_and_faster_itoa2() {
        let i64_ref = |n: i64| {
            let mut v = Vec::new();
            push_i64(&mut v, n);
            assert_eq!(v, n.to_string().into_bytes(), "push_i64 wrong for {n}");
        };
        let usize_ref = |n: usize| {
            let mut v = Vec::new();
            push_usize(&mut v, n);
            assert_eq!(v, n.to_string().into_bytes(), "push_usize wrong for {n}");
        };

        // Exhaustive small range (covers 0, single, two, three digits + carries).
        for n in 0..=20_000i64 {
            i64_ref(n);
            i64_ref(-n);
            usize_ref(n as usize);
        }
        // Power-of-ten and 9.. boundaries where 2-digit chunking changes parity.
        let mut p: u64 = 1;
        for _ in 0..19 {
            for d in [p.wrapping_sub(1), p, p.wrapping_add(1)] {
                usize_ref(d as usize);
                if d <= i64::MAX as u64 {
                    i64_ref(d as i64);
                    i64_ref(-(d as i64));
                }
            }
            p = p.saturating_mul(10);
        }
        // i64 / u64 / usize extremes.
        i64_ref(i64::MAX);
        i64_ref(i64::MIN);
        usize_ref(usize::MAX);
        usize_ref(u64::MAX as usize);

        // Deterministic random sweep.
        let mut s: u64 = 0xD1B54A32D192ED03;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..200_000 {
            let r = next();
            usize_ref(r as usize);
            i64_ref(r as i64);
        }

        if cfg!(debug_assertions) {
            return;
        }
        // ---- Score: old digit-at-a-time div loop vs new two-digit LUT ----
        fn old_push_usize(out: &mut Vec<u8>, n: usize) {
            if n == 0 {
                out.push(b'0');
                return;
            }
            let mut val = n;
            let mut buf = [0u8; 20];
            let mut pos = 20;
            while val > 0 {
                pos -= 1;
                buf[pos] = b'0' + (val % 10) as u8;
                val /= 10;
            }
            out.extend_from_slice(&buf[pos..]);
        }
        // Realistic reply-integer mix: ~half small length-headers (0..10000),
        // ~half full-range values (counters, large LLEN/SCARD/INCR results).
        let mut s2: u64 = 0x9E3779B97F4A7C15;
        let mut nx = || {
            s2 ^= s2 << 13;
            s2 ^= s2 >> 7;
            s2 ^= s2 << 17;
            s2
        };
        let nums: Vec<usize> = (0..4096)
            .map(|i| {
                let r = nx();
                if i % 2 == 0 {
                    (r % 10_000) as usize
                } else {
                    r as usize
                }
            })
            .collect();
        let reps = 20_000;
        let mut sink = Vec::with_capacity(64);
        let t0 = std::time::Instant::now();
        let mut acc = 0usize;
        for _ in 0..reps {
            for &n in &nums {
                sink.clear();
                old_push_usize(&mut sink, n);
                acc = acc.wrapping_add(sink.len());
            }
        }
        let old_ns = t0.elapsed().as_nanos().max(1);
        std::hint::black_box(acc);
        let t1 = std::time::Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..reps {
            for &n in &nums {
                sink.clear();
                push_usize(&mut sink, n);
                acc2 = acc2.wrapping_add(sink.len());
            }
        }
        let new_ns = t1.elapsed().as_nanos().max(1);
        std::hint::black_box(acc2);
        assert_eq!(acc, acc2, "old/new total digit length disagree");
        let score = old_ns as f64 / new_ns as f64;
        eprintln!("ITOA2 reply-int mix: old={old_ns}ns new={new_ns}ns score={score:.2}x");
        // Measured ~1.58x real-world (format-into-Vec; the common extend cost
        // caps the kernel speedup per Amdahl). This is a regression floor, not a
        // 2.0 extreme-opt claim — guards that the two-digit LUT stays a net win.
        assert!(
            score >= 1.25,
            "two-digit LUT itoa regressed below floor; got {score:.2}x"
        );
    }

    // (frankenredis-e4fu8) Bench guard for the branchless ilog10 digit-count vs the
    // original div-by-10 loop, on the same realistic reply-integer mix. Correctness is
    // covered by decimal_len_matches_div_loop_reference; this asserts ilog10 stays a net
    // win (conservative floor — timing-noise tolerant, real ratio in the eprintln). The
    // batch release run records the true score; this only catches a real regression.
    #[test]
    fn decimal_len_ilog10_not_slower_than_div_loop() {
        if cfg!(debug_assertions) {
            return;
        }
        fn old_decimal_usize_len(mut n: usize) -> usize {
            let mut len = 1;
            while n >= 10 {
                n /= 10;
                len += 1;
            }
            len
        }
        let mut s: u64 = 0x2545F4914F6CDD1D;
        let mut nx = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let nums: Vec<usize> = (0..4096)
            .map(|i| {
                let r = nx();
                if i % 2 == 0 {
                    (r % 10_000) as usize
                } else {
                    r as usize
                }
            })
            .collect();
        let reps = 50_000;
        let t0 = std::time::Instant::now();
        let mut acc = 0usize;
        for _ in 0..reps {
            for &n in &nums {
                acc = acc.wrapping_add(old_decimal_usize_len(n));
            }
        }
        let old_ns = t0.elapsed().as_nanos().max(1);
        std::hint::black_box(acc);
        let t1 = std::time::Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..reps {
            for &n in &nums {
                acc2 = acc2.wrapping_add(decimal_usize_len(n));
            }
        }
        let new_ns = t1.elapsed().as_nanos().max(1);
        std::hint::black_box(acc2);
        assert_eq!(acc, acc2, "old/new digit-count disagree");
        let score = old_ns as f64 / new_ns as f64;
        eprintln!(
            "ILOG10 decimal-len reply-int mix: old={old_ns}ns new={new_ns}ns score={score:.2}x"
        );
        // Conservative regression floor (not a speedup claim — ilog10 is reasoned-faster;
        // the real ratio is in the eprintln). 0.85 only trips on a true regression.
        assert!(
            score >= 0.85,
            "ilog10 decimal-len regressed; got {score:.2}x"
        );
    }

    #[test]
    fn parse_command_frame_requires_bulk_elements() {
        // (frankenredis-5qqv1) A command multibulk's elements must each be a
        // non-null bulk string, matching upstream processMultibulkBuffer.
        let cfg = ParserConfig::default();

        // Valid command parses normally.
        let ok = parse_command_frame(b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n", &cfg).unwrap();
        assert_eq!(
            ok.frame,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(b"k".to_vec())),
            ]))
        );

        // Non-`$` element -> ExpectedBulk carrying the offending type byte.
        for (input, got) in [
            (&b"*1\r\n+PING\r\n"[..], b'+'),
            (b"*1\r\n:5\r\n", b':'),
            (b"*1\r\n*0\r\n", b'*'),
        ] {
            assert_eq!(
                parse_command_frame(input, &cfg).unwrap_err(),
                RespParseError::ExpectedBulk(got),
                "input {input:?}"
            );
        }

        // A `$-1` null bulk is a valid reply but never a command argument.
        assert_eq!(
            parse_command_frame(b"*2\r\n$3\r\nGET\r\n$-1\r\n", &cfg).unwrap_err(),
            RespParseError::InvalidBulkLength
        );

        // `expected '$', got 'X'` wording matches upstream.
        assert_eq!(
            RespParseError::ExpectedBulk(b'+').to_string(),
            "expected '$', got '+'"
        );
    }

    #[test]
    fn parse_command_frame_borrowed_matches_owned_command_parser() {
        let cfg = ParserConfig::default();
        let input = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n+tail\r\n";
        let owned = parse_command_frame(input, &cfg).expect("owned command parses");
        let borrowed = parse_command_frame_borrowed(input, &cfg).expect("borrowed command parses");

        assert_eq!(borrowed.consumed, owned.consumed);
        assert_eq!(borrowed.consumed, input.len() - b"+tail\r\n".len());
        assert_eq!(
            borrowed.frame,
            BorrowedCommandFrame::Arguments(vec![
                b"SET".as_slice(),
                b"key".as_slice(),
                b"value".as_slice(),
            ])
        );
    }

    #[test]
    fn parse_command_frame_borrowed_preserves_empty_and_null_multibulks() {
        let cfg = ParserConfig::default();

        let empty = parse_command_frame_borrowed(b"*0\r\n", &cfg).expect("empty array parses");
        assert_eq!(empty.consumed, 4);
        assert_eq!(empty.frame, BorrowedCommandFrame::Arguments(Vec::new()));

        let null = parse_command_frame_borrowed(b"*-1\r\n", &cfg).expect("null array parses");
        assert_eq!(null.consumed, 5);
        assert_eq!(null.frame, BorrowedCommandFrame::NullArray);
    }

    #[test]
    fn parse_command_frame_borrowed_preserves_fallback_and_error_semantics() {
        let cfg = ParserConfig::default();

        let simple = parse_command_frame_borrowed(b"+OK\r\n", &cfg).expect("fallback parses");
        assert_eq!(simple.consumed, 5);
        assert_eq!(
            simple.frame,
            BorrowedCommandFrame::Owned(RespFrame::SimpleString("OK".to_string()))
        );

        for (input, expected) in [
            (&b"*1\r\n+PING\r\n"[..], RespParseError::ExpectedBulk(b'+')),
            (&b"*1\r\n:5\r\n"[..], RespParseError::ExpectedBulk(b':')),
            (&b"*1\r\n*0\r\n"[..], RespParseError::ExpectedBulk(b'*')),
            (
                &b"*2\r\n$3\r\nGET\r\n$-1\r\n"[..],
                RespParseError::InvalidBulkLength,
            ),
            (
                &b"*2\r\n$3\r\nGET\r\n$4\r\nkey\n"[..],
                RespParseError::Incomplete,
            ),
        ] {
            assert_eq!(
                parse_command_frame_borrowed(input, &cfg).unwrap_err(),
                expected,
                "input {input:?}"
            );
            assert_eq!(
                parse_command_frame(input, &cfg).unwrap_err(),
                expected,
                "owned input {input:?}"
            );
        }
    }

    #[test]
    fn oversized_header_lines_use_context_specific_wording() {
        // (frankenredis-linetoolong-wording) Upstream emits context-specific
        // "too big mbulk/bulk count string" for an over-cap command header line,
        // not the generic LineTooLong. Build a >MAX_LINE_LENGTH line with no
        // terminator at each header position.
        let cfg = ParserConfig::default();
        let big = vec![b'9'; MAX_LINE_LENGTH + 16];
        // *<count> line too long.
        let mut count_line = vec![b'*'];
        count_line.extend_from_slice(&big);
        assert_eq!(
            parse_command_args_borrowed_into(&count_line, &cfg, &mut Vec::new()).unwrap_err(),
            RespParseError::TooBigMbulkCount
        );
        assert_eq!(
            parse_command_frame(&count_line, &cfg).unwrap_err(),
            RespParseError::TooBigMbulkCount
        );
        // $<len> element-header line too long.
        let mut bulk_hdr = b"*1\r\n$".to_vec();
        bulk_hdr.extend_from_slice(&big);
        assert_eq!(
            parse_command_args_borrowed_into(&bulk_hdr, &cfg, &mut Vec::new()).unwrap_err(),
            RespParseError::TooBigBulkCount
        );
        // Non-`$` element line too long.
        let mut bad_elem = b"*1\r\n".to_vec();
        bad_elem.extend_from_slice(&vec![b'Z'; MAX_LINE_LENGTH + 16]);
        assert_eq!(
            parse_command_args_borrowed_into(&bad_elem, &cfg, &mut Vec::new()).unwrap_err(),
            RespParseError::TooBigBulkCount
        );
    }

    #[test]
    fn multibulk_bad_element_defers_error_until_line_complete() {
        // (frankenredis-mbulkdefer) Upstream processMultibulkBuffer locates an
        // element's line terminator BEFORE checking its type byte: a malformed
        // (non-`$`) element whose line hasn't fully arrived must return
        // Incomplete (wait), and only error once the `\r\n` is present — both
        // for the borrowed (live) and owned parsers.
        let cfg = ParserConfig::default();
        let cases = [
            // (input, expected): no terminator yet -> Incomplete (wait); with the
            // `\r\n` present -> the type-byte error stands.
            (&b"*1\r\nPING"[..], RespParseError::Incomplete),
            (
                &b"*2\r\n$4\r\nPING\r\nGARBAGE"[..],
                RespParseError::Incomplete,
            ),
            (&b"*1\r\nPING\r\n"[..], RespParseError::ExpectedBulk(b'P')),
            (
                &b"*2\r\n$4\r\nPING\r\nGARBAGE\r\n"[..],
                RespParseError::ExpectedBulk(b'G'),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_command_frame_borrowed(input, &cfg).unwrap_err(),
                expected,
                "borrowed input {input:?}"
            );
            assert_eq!(
                parse_command_frame(input, &cfg).unwrap_err(),
                expected,
                "owned input {input:?}"
            );
        }
    }

    #[test]
    fn parse_command_args_borrowed_into_reuses_buffer_and_clears_on_error() {
        let cfg = ParserConfig::default();
        let mut args = Vec::with_capacity(8);
        let capacity = args.capacity();
        let input = b"*3\r\n$3\r\nSET\r\n$0\r\n\r\n$6\r\n\x00\xff\r\nzz\r\n";

        let parsed =
            parse_command_args_borrowed_into(input, &cfg, &mut args).expect("borrowed parses");
        assert_eq!(parsed.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(parsed.consumed, input.len());
        assert_eq!(
            args,
            vec![
                b"SET".as_slice(),
                b"".as_slice(),
                b"\x00\xff\r\nzz".as_slice()
            ]
        );
        assert_eq!(args.capacity(), capacity);

        let err = parse_command_args_borrowed_into(b"*1\r\n+PING\r\n", &cfg, &mut args)
            .expect_err("non-bulk arg must reject");
        assert_eq!(err, RespParseError::ExpectedBulk(b'+'));
        assert!(args.is_empty());
    }

    #[test]
    fn command_bulk_arg_skips_mismatched_terminator_like_upstream() {
        // (frankenredis-v4cl4) A multibulk bulk arg whose declared length does
        // not match its content must NOT be rejected: upstream advances
        // `qb_pos += bulklen+2` blindly, so `$3\r\nPING\r\n` reads "PIN" and the
        // trailing "G\r" is consumed as the (unvalidated) terminator, leaving
        // "\n" for the next frame. Both command parsers must agree, and reply
        // parsing must stay strict.
        let cfg = ParserConfig::default();

        // Owned command parser: reads 3 bytes "PIN", consumes 3+2 more.
        let parsed = parse_command_frame(b"*1\r\n$3\r\nPING\r\n", &cfg).expect("lenient command");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PIN".to_vec()))]))
        );
        assert_eq!(parsed.consumed, b"*1\r\n$3\r\nPING\r".len()); // up to but not incl trailing "\n"

        // Borrowed command parser agrees (same arg, same consumed offset).
        let mut args = Vec::new();
        let borrowed = parse_command_args_borrowed_into(b"*1\r\n$3\r\nPING\r\n", &cfg, &mut args)
            .expect("lenient borrowed command");
        assert_eq!(args, vec![&b"PIN"[..]]);
        assert_eq!(borrowed.consumed, parsed.consumed);

        // Two args: `$2 key` reads "ke" and skips "y\r".
        let parsed = parse_command_frame(b"*2\r\n$3\r\nGET\r\n$2\r\nkey\r\n", &cfg).expect("ok");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(b"ke".to_vec())),
            ]))
        );

        // Reply parsing stays STRICT: the generic parser still rejects it.
        assert_eq!(
            parse_frame_with_config(b"$3\r\nPING\r\n", &cfg).unwrap_err(),
            RespParseError::InvalidBulkLength
        );

        // Still need bulklen+2 bytes available, else Incomplete.
        assert_eq!(
            parse_command_frame(b"*1\r\n$3\r\nfoo\r", &cfg).unwrap_err(),
            RespParseError::Incomplete
        );
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_multibulk_error_semantics() {
        let cfg = ParserConfig::default();
        let allow_resp3 = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let cases = [
            &b"*1\r\n+PING\r\n"[..],
            &b"*1\r\n:5\r\n"[..],
            &b"*1\r\n_ \r\n"[..],
            &b"*1\r\n#t\r\n"[..],
            &b"*1\r\n*0\r\n"[..],
            &b"*2\r\n$3\r\nGET\r\n$-1\r\n"[..],
            &b"*x\r\n"[..],
            &b"*1\r\n$3\r\nfo"[..],
            &b"*1\r\n$01\r\nx\r\n"[..],
            &b"*1\r\n$-0\r\n"[..],
        ];
        let mut args = Vec::new();
        for input in cases {
            assert_eq!(
                parse_command_args_borrowed_into(input, &cfg, &mut args).unwrap_err(),
                parse_command_frame(input, &cfg).unwrap_err(),
                "default config input {input:?}"
            );
            assert!(
                args.is_empty(),
                "default config left stale args for {input:?}"
            );
            assert_eq!(
                parse_command_args_borrowed_into(input, &allow_resp3, &mut args).unwrap_err(),
                parse_command_frame(input, &allow_resp3).unwrap_err(),
                "allow_resp3 input {input:?}"
            );
            assert!(args.is_empty(), "allow_resp3 left stale args for {input:?}");
        }
        // (frankenredis-6dpyk) A negative multibulk count <= 0 (e.g. *-2) is a
        // no-op upstream, NOT an error — both command parsers must agree it is
        // the null-array form, not surface InvalidMultibulkLength.
        assert!(matches!(
            parse_command_frame(b"*-2\r\n", &cfg).unwrap().frame,
            RespFrame::Array(None)
        ));
        assert!(parse_command_args_borrowed_into(b"*-2\r\n", &cfg, &mut args).is_ok());
        assert!(parse_command_frame(b"*-9\r\n", &cfg).unwrap().frame == RespFrame::Array(None));
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_config_limits() {
        let mut args = Vec::new();
        let bulk_limited = ParserConfig {
            max_bulk_len: 4,
            ..ParserConfig::default()
        };
        let at_limit =
            parse_command_args_borrowed_into(b"*1\r\n$4\r\nxxxx\r\n", &bulk_limited, &mut args)
                .expect("bulk at max_bulk_len parses");
        assert_eq!(at_limit.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(args, vec![b"xxxx".as_slice()]);
        assert_eq!(
            parse_command_args_borrowed_into(b"*1\r\n$5\r\nxxxxx\r\n", &bulk_limited, &mut args)
                .unwrap_err(),
            RespParseError::BulkLengthTooLarge
        );

        let array_limited = ParserConfig {
            max_array_len: 1,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_command_args_borrowed_into(
                b"*2\r\n$1\r\na\r\n$1\r\nb\r\n",
                &array_limited,
                &mut args
            )
            .unwrap_err(),
            RespParseError::MultibulkLengthTooLarge
        );
    }

    #[test]
    fn parse_command_args_borrowed_into_preserves_null_and_empty_array_status() {
        let cfg = ParserConfig::default();
        let mut args = Vec::new();

        let empty = parse_command_args_borrowed_into(b"*0\r\n", &cfg, &mut args)
            .expect("empty array parses");
        assert_eq!(empty.kind, BorrowedCommandArgsKind::Arguments);
        assert_eq!(empty.consumed, 4);
        assert!(args.is_empty());

        let null = parse_command_args_borrowed_into(b"*-1\r\n", &cfg, &mut args)
            .expect("null array parses");
        assert_eq!(null.kind, BorrowedCommandArgsKind::NullArray);
        assert_eq!(null.consumed, 5);
        assert!(args.is_empty());
    }

    const PACKET_ID: &str = "FR-P2C-002";
    const SCHEMA_VERSION: &str = "fr_testlog_v1";
    const ARTIFACT_REFS: [&str; 4] = [
        "TEST_LOG_SCHEMA_V1.md",
        "crates/fr-conformance/fixtures/phase2c/FR-P2C-002/contract_table.md",
        "crates/fr-conformance/fixtures/phase2c/FR-P2C-002/risk_note.md",
        "crates/fr-conformance/fixtures/log_contract_v1/env.json",
    ];

    #[derive(Debug)]
    struct StructuredTestLogEvent {
        schema_version: String,
        ts_utc: String,
        suite_id: String,
        test_or_scenario_id: String,
        packet_id: String,
        mode: String,
        verification_path: String,
        seed: u64,
        input_digest: String,
        output_digest: String,
        duration_ms: u64,
        outcome: String,
        reason_code: String,
        replay_cmd: String,
        artifact_refs: Vec<String>,
        fixture_id: Option<String>,
        env_ref: Option<String>,
    }

    impl StructuredTestLogEvent {
        fn assert_schema_contract(&self) {
            assert_eq!(self.schema_version, SCHEMA_VERSION);
            assert_eq!(self.packet_id, PACKET_ID);
            assert!(!self.ts_utc.is_empty());
            assert!(self.suite_id.starts_with("unit::fr-p2c-002"));
            assert!(self.test_or_scenario_id.starts_with("fr_p2c_002_"));
            assert_eq!(self.mode, "strict");
            assert!(matches!(
                self.verification_path.as_str(),
                "unit" | "property"
            ));
            assert!(self.seed > 0);
            assert!(!self.input_digest.is_empty());
            assert!(!self.output_digest.is_empty());
            assert!(self.duration_ms > 0);
            assert_eq!(self.outcome, "pass");
            assert!(!self.reason_code.is_empty());
            assert!(self.replay_cmd.contains("cargo test -p fr-protocol"));
            assert!(self.replay_cmd.contains(&self.test_or_scenario_id));
            assert!(!self.artifact_refs.is_empty());
            for required in ARTIFACT_REFS {
                assert!(
                    self.artifact_refs
                        .iter()
                        .any(|artifact| artifact == required),
                    "missing required artifact ref: {required}"
                );
            }
            assert_eq!(
                self.fixture_id.as_deref(),
                Some("FR-P2C-002::unit-contract-fixture")
            );
            assert_eq!(
                self.env_ref.as_deref(),
                Some("crates/fr-conformance/fixtures/log_contract_v1/env.json")
            );
        }
    }

    fn stable_digest_hex(bytes: &[u8]) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    fn build_event(
        test_or_scenario_id: &str,
        verification_path: &str,
        seed: u64,
        input_bytes: &[u8],
        output_bytes: &[u8],
        reason_code: &str,
    ) -> StructuredTestLogEvent {
        StructuredTestLogEvent {
            schema_version: SCHEMA_VERSION.to_string(),
            ts_utc: "2026-02-16T00:00:00Z".to_string(),
            suite_id: "unit::fr-p2c-002".to_string(),
            test_or_scenario_id: test_or_scenario_id.to_string(),
            packet_id: PACKET_ID.to_string(),
            mode: "strict".to_string(),
            verification_path: verification_path.to_string(),
            seed,
            input_digest: stable_digest_hex(input_bytes),
            output_digest: stable_digest_hex(output_bytes),
            duration_ms: 1,
            outcome: "pass".to_string(),
            reason_code: reason_code.to_string(),
            replay_cmd: format!(
                "FR_MODE=strict FR_SEED={seed} cargo test -p fr-protocol {test_or_scenario_id} -- --nocapture"
            ),
            artifact_refs: ARTIFACT_REFS.into_iter().map(str::to_string).collect(),
            fixture_id: Some("FR-P2C-002::unit-contract-fixture".to_string()),
            env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string()),
        }
    }

    fn nested_singleton_array(depth: usize) -> RespFrame {
        let mut frame = RespFrame::Integer(42);
        for _ in 0..depth {
            frame = RespFrame::Array(Some(vec![frame]));
        }
        frame
    }

    #[test]
    fn fr_p2c_002_u001_scalar_decode_parity() {
        let cases = [
            (
                b"+OK\r\n".as_slice(),
                RespFrame::SimpleString("OK".to_string()),
            ),
            (
                b"-ERR boom\r\n".as_slice(),
                RespFrame::Error("ERR boom".to_string()),
            ),
            (b":-42\r\n".as_slice(), RespFrame::Integer(-42)),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected) in cases {
            let parsed = parse_frame(input).expect("scalar frame must parse");
            assert_eq!(parsed.frame, expected);
            assert_eq!(parsed.consumed, input.len());
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u001_scalar_decode_parity",
            "unit",
            17,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn resp_inline_encoder_sanitizes_crlf_in_body() {
        // CRLF-injection guard: when SimpleString/Error bodies are
        // built via `format!()` from user-controlled bytes (e.g.
        // `RespFrame::Error(format!("ERR Unrecognized option '{attr}'"))`
        // where `attr` comes from a client argv), any embedded `\r`
        // or `\n` would terminate the inline frame early and let the
        // remaining bytes be re-parsed by the peer as a separate
        // reply. The encoder must replace `\r` and `\n` with spaces,
        // matching upstream Redis' `_addReplyErrorFormat`
        // sanitization.
        let injected = RespFrame::Error("ERR x\r\nINJECTED".to_string());
        let bytes = injected.to_bytes();
        assert_eq!(
            bytes, b"-ERR x  INJECTED\r\n",
            "Error body must have \\r and \\n replaced with spaces; otherwise the frame splits"
        );
        // And re-parsing the encoder output round-trips through our
        // own parser as a single Error frame consuming all bytes —
        // proving the injection is neutralized.
        let parsed = parse_frame(&bytes).expect("sanitized frame must parse");
        assert_eq!(parsed.consumed, bytes.len());
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR x  INJECTED".to_string())
        );

        let ss = RespFrame::SimpleString("OK\r\nSMUGGLED".to_string());
        let ss_bytes = ss.to_bytes();
        assert_eq!(ss_bytes, b"+OK  SMUGGLED\r\n");
        let parsed_ss = parse_frame(&ss_bytes).expect("sanitized SimpleString must parse");
        assert_eq!(parsed_ss.consumed, ss_bytes.len());

        // Lone \r and lone \n in isolation must also be sanitized —
        // a peer reading bytes off the wire could see a lone \n as a
        // legacy inline-protocol terminator.
        let lone = RespFrame::Error("ERR a\rb\nc".to_string()).to_bytes();
        assert_eq!(lone, b"-ERR a b c\r\n");
    }

    #[test]
    fn resp_integer_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b":+1\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
        assert!(matches!(
            parse_frame(b":01\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
        assert!(matches!(
            parse_frame(b":-0\r\n"),
            Err(RespParseError::InvalidInteger)
        ));
    }

    #[test]
    fn resp_bulk_len_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b"$+1\r\nx\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
        assert!(matches!(
            parse_frame(b"$01\r\nx\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
        assert!(matches!(
            parse_frame(b"$-0\r\n"),
            Err(RespParseError::InvalidBulkLength)
        ));
    }

    #[test]
    fn resp_array_len_rejects_noncanonical_tokens() {
        assert!(matches!(
            parse_frame(b"*+1\r\n$1\r\na\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
        assert!(matches!(
            parse_frame(b"*01\r\n$1\r\na\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
        assert!(matches!(
            parse_frame(b"*-0\r\n"),
            Err(RespParseError::InvalidMultibulkLength)
        ));
    }

    #[test]
    fn fr_p2c_002_u002_bulk_decode_parity() {
        let cases = [
            (
                b"$5\r\nhello\r\n".as_slice(),
                RespFrame::BulkString(Some(b"hello".to_vec())),
            ),
            (
                b"$4\r\n\x00\xff\x10z\r\n".as_slice(),
                RespFrame::BulkString(Some(vec![0x00, 0xff, 0x10, b'z'])),
            ),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected) in cases {
            let parsed = parse_frame(input).expect("bulk frame must parse");
            assert_eq!(parsed.frame, expected);
            assert_eq!(parsed.consumed, input.len());
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u002_bulk_decode_parity",
            "unit",
            19,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u003_array_recursion_parity_property() {
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for depth in 0..=8 {
            let frame = nested_singleton_array(depth);
            let encoded = frame.to_bytes();
            let parsed = parse_frame(encoded.as_slice()).expect("recursive array must parse");
            assert_eq!(parsed.frame, frame);
            assert_eq!(parsed.consumed, encoded.len());
            input_acc.extend_from_slice(encoded.as_slice());
            output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        }
        let event = build_event(
            "fr_p2c_002_u003_array_recursion_parity_property",
            "property",
            23,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u004_truncated_frame_rejection() {
        let cases = [
            b"+OK\r".as_slice(),
            b"$3\r\nab".as_slice(),
            b"*2\r\n+OK\r\n".as_slice(),
            b":123".as_slice(),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for input in cases {
            let err = parse_frame(input).expect_err("truncated frame must fail");
            assert_eq!(err, RespParseError::Incomplete);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u004_truncated_frame_rejection",
            "unit",
            29,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.incomplete_frame_detected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u005_malformed_length_rejection() {
        let cases = [
            (b"$x\r\n".as_slice(), RespParseError::InvalidBulkLength),
            (b"$-2\r\n".as_slice(), RespParseError::InvalidBulkLength),
            (
                b"$9223372036854775808\r\n".as_slice(),
                RespParseError::InvalidBulkLength,
            ),
            (b"*x\r\n".as_slice(), RespParseError::InvalidMultibulkLength),
            (
                b"*-2\r\n".as_slice(),
                RespParseError::InvalidMultibulkLength,
            ),
            (
                b"*9223372036854775808\r\n".as_slice(),
                RespParseError::InvalidMultibulkLength,
            ),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected_err) in cases {
            let err = parse_frame(input).expect_err("malformed length must fail");
            assert_eq!(err, expected_err);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u005_malformed_length_rejection",
            "unit",
            31,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.invalid_length_rejected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u005_line_length_limit_is_inclusive() {
        let mut ok = Vec::with_capacity(MAX_LINE_LENGTH + 3);
        ok.push(b'+');
        ok.extend(std::iter::repeat_n(b'a', MAX_LINE_LENGTH));
        ok.extend_from_slice(b"\r\n");
        let parsed = parse_frame(ok.as_slice()).expect("line at limit must parse");
        assert_eq!(
            parsed.frame,
            RespFrame::SimpleString("a".repeat(MAX_LINE_LENGTH))
        );

        let mut too_long = Vec::with_capacity(MAX_LINE_LENGTH + 4);
        too_long.push(b'+');
        too_long.extend(std::iter::repeat_n(b'a', MAX_LINE_LENGTH + 1));
        too_long.extend_from_slice(b"\r\n");
        let err = parse_frame(too_long.as_slice()).expect_err("line beyond limit must fail");
        assert_eq!(err, RespParseError::LineTooLong);
    }

    #[test]
    fn fr_p2c_002_u006_invalid_prefix_rejection() {
        let cases = *b"?@/";
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for prefix in cases {
            let input = [prefix, b'\r', b'\n'];
            let err = parse_frame(input.as_slice()).expect_err("unknown prefix must fail");
            assert_eq!(err, RespParseError::InvalidPrefix(prefix));
            input_acc.extend_from_slice(input.as_slice());
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u006_invalid_prefix_rejection",
            "unit",
            37,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.invalid_prefix_rejected",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u007_resp3_fail_closed_prefix_matrix() {
        let prefixes = *b"~%#,_(=|>!";
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for prefix in prefixes {
            let input = [prefix, b'1', b'\r', b'\n'];
            let err = parse_frame(input.as_slice()).expect_err("unsupported RESP3 must fail");
            assert_eq!(err, RespParseError::UnsupportedResp3Type(prefix));
            input_acc.extend_from_slice(input.as_slice());
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u007_resp3_fail_closed_prefix_matrix",
            "unit",
            41,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.resp3_unimplemented_fail_closed",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u008_attribute_cursor_alignment_fail_closed() {
        let attr_wrapped = b"|1\r\n+meta\r\n+value\r\n+OK\r\n";
        let err = parse_frame(attr_wrapped).expect_err("attribute wrapper must fail closed");
        assert_eq!(err, RespParseError::UnsupportedResp3Type(b'|'));

        let follow_up = parse_frame(b"+OK\r\n").expect("independent parse remains deterministic");
        assert_eq!(follow_up.frame, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(follow_up.consumed, 5);

        let event = build_event(
            "fr_p2c_002_u008_attribute_cursor_alignment_fail_closed",
            "unit",
            43,
            attr_wrapped,
            err.to_string().as_bytes(),
            "protocol.attribute_cursor_drift",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u009_consumed_length_exactness_property() {
        let frames = [
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(7),
            RespFrame::BulkString(Some(b"hello".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"PING".to_vec())),
                RespFrame::BulkString(Some(b"payload".to_vec())),
            ])),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for frame in frames {
            let encoded = frame.to_bytes();
            for tail_len in 0..=4 {
                let mut with_tail = encoded.clone();
                with_tail.extend(std::iter::repeat_n(b'X', tail_len));
                let parsed = parse_frame(with_tail.as_slice()).expect("frame with tail must parse");
                assert_eq!(parsed.frame, frame);
                assert_eq!(parsed.consumed, encoded.len());
                input_acc.extend_from_slice(with_tail.as_slice());
                output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
            }
        }
        let event = build_event(
            "fr_p2c_002_u009_consumed_length_exactness_property",
            "property",
            47,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "parity_ok",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u010_null_semantics_parity() {
        let null_bulk = parse_frame(b"$-1\r\n").expect("canonical null bulk");
        assert_eq!(null_bulk.frame, RespFrame::BulkString(None));
        assert_eq!(null_bulk.consumed, 5);

        let null_array = parse_frame(b"*-1\r\n").expect("canonical null array");
        assert_eq!(null_array.frame, RespFrame::Array(None));
        assert_eq!(null_array.consumed, 5);

        let noncanonical_bulk = parse_frame(b"$-01\r\n").expect_err("must reject non-canonical");
        assert_eq!(noncanonical_bulk, RespParseError::InvalidBulkLength);
        let noncanonical_array = parse_frame(b"*-01\r\n").expect_err("must reject non-canonical");
        assert_eq!(noncanonical_array, RespParseError::InvalidMultibulkLength);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"$-1\r\n");
        input_acc.extend_from_slice(b"*-1\r\n");
        input_acc.extend_from_slice(b"$-01\r\n");
        input_acc.extend_from_slice(b"*-01\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(null_bulk.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(null_array.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(noncanonical_bulk.to_string().as_bytes());
        output_acc.extend_from_slice(noncanonical_array.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u010_null_semantics_parity",
            "unit",
            53,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.null_semantics_drift",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn encode_into_resp3_promotes_nulls_and_recurses() {
        // (frankenredis-pgplm) Redis 7.2 emits the RESP3 null type `_\r\n`
        // for every null under HELLO 3 — bulk, array, map, set — never the
        // RESP2 `$-1` / `*-1`. encode_into_resp3 promotes null leaves and
        // recurses into containers; non-null scalars are byte-identical to
        // encode_into.
        fn r3(frame: &RespFrame) -> Vec<u8> {
            let mut out = Vec::new();
            frame.encode_into_resp3(&mut out);
            out
        }
        assert_eq!(r3(&RespFrame::BulkString(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Array(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Map(None)), b"_\r\n");
        assert_eq!(r3(&RespFrame::Set(None)), b"_\r\n");

        // Nested nulls (MGET-style array with a hit and two misses).
        let mget = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"1".to_vec())),
            RespFrame::BulkString(None),
            RespFrame::BulkString(None),
        ]));
        assert_eq!(r3(&mget), b"*3\r\n$1\r\n1\r\n_\r\n_\r\n");

        // XPENDING-style summary: count + three nulls (min/max/consumers).
        let xpending = RespFrame::Array(Some(vec![
            RespFrame::Integer(0),
            RespFrame::BulkString(None),
            RespFrame::BulkString(None),
            RespFrame::Array(None),
        ]));
        assert_eq!(r3(&xpending), b"*4\r\n:0\r\n_\r\n_\r\n_\r\n");

        // Non-null scalars and populated containers are identical to RESP2
        // encoding (only the null leaves differ).
        for frame in [
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(42),
            RespFrame::BulkString(Some(b"hi".to_vec())),
            RespFrame::Double("1.5".to_string()),
            RespFrame::Array(Some(vec![RespFrame::Integer(1)])),
        ] {
            let mut a = Vec::new();
            frame.encode_into(&mut a);
            assert_eq!(r3(&frame), a, "non-null frame must encode identically");
        }
    }

    #[test]
    fn attribute_and_bool_frames_encode_resp3_wire_forms_01weh() {
        // (frankenredis-0gz4g / 01weh) RESP3 Bool encodes `#t`/`#f`; the RESP3
        // Attribute encodes `|count\r\n` followed by its pairs (same body shape
        // as a Map but the `|` prefix). Both encode identically via encode_into
        // and encode_into_resp3.
        let r2 = |f: &RespFrame| {
            let mut o = Vec::new();
            f.encode_into(&mut o);
            o
        };
        let r3 = |f: &RespFrame| {
            let mut o = Vec::new();
            f.encode_into_resp3(&mut o);
            o
        };
        assert_eq!(r2(&RespFrame::Bool(true)), b"#t\r\n");
        assert_eq!(r3(&RespFrame::Bool(false)), b"#f\r\n");
        // DEBUG PROTOCOL attrib's attribute block.
        let attr = RespFrame::Attribute(vec![(
            RespFrame::BulkString(Some(b"key-popularity".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"key:123".to_vec())),
                RespFrame::Integer(90),
            ])),
        )]);
        let expected = b"|1\r\n$14\r\nkey-popularity\r\n*2\r\n$7\r\nkey:123\r\n:90\r\n".to_vec();
        assert_eq!(r2(&attr), expected);
        assert_eq!(r3(&attr), expected);
    }

    #[test]
    fn fr_p2c_002_u011_invalid_utf8_consistency() {
        // Test that invalid UTF-8 in scalar frames is rejected appropriately.
        // Simple strings and errors should fail with InvalidUtf8, while integers
        // fail with InvalidInteger (0xFF is not a valid digit).
        let cases: [(&[u8], RespParseError); 3] = [
            (b"+\xff\r\n", RespParseError::InvalidUtf8),
            (b"-\xff\r\n", RespParseError::InvalidUtf8),
            (b":\xff\r\n", RespParseError::InvalidInteger),
        ];
        let mut input_acc = Vec::new();
        let mut output_acc = Vec::new();
        for (input, expected_err) in cases {
            let err = parse_frame(input).expect_err("invalid scalar must fail");
            assert_eq!(err, expected_err);
            input_acc.extend_from_slice(input);
            output_acc.extend_from_slice(err.to_string().as_bytes());
        }
        let event = build_event(
            "fr_p2c_002_u011_invalid_utf8_consistency",
            "unit",
            59,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.scalar_decode_mismatch",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u012_depth_or_size_stress_behavior_is_deterministic() {
        let deep_frame = nested_singleton_array(64);
        let encoded = deep_frame.to_bytes();
        let parsed = parse_frame(encoded.as_slice()).expect("deep frame must parse");
        assert_eq!(parsed.frame, deep_frame);
        assert_eq!(parsed.consumed, encoded.len());

        let truncated = &encoded[..encoded.len() - 2];
        let truncated_err = parse_frame(truncated).expect_err("truncated deep frame must fail");
        assert_eq!(truncated_err, RespParseError::Incomplete);

        let oversized = b"$100\r\nabc\r\n";
        let oversized_err = parse_frame(oversized).expect_err("short payload must fail");
        assert_eq!(oversized_err, RespParseError::Incomplete);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(encoded.as_slice());
        input_acc.extend_from_slice(truncated);
        input_acc.extend_from_slice(oversized);

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(parsed.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(truncated_err.to_string().as_bytes());
        output_acc.extend_from_slice(oversized_err.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u012_depth_or_size_stress_behavior_is_deterministic",
            "property",
            61,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.depth_or_size_resource_clamp",
        );
        event.assert_schema_contract();
    }

    /// Opt-in RESP3 parser (br-frankenredis-ozcx). Default config
    /// still hard-rejects every RESP3 prefix; with allow_resp3=true
    /// each RESP3 type downgrades into a RESP2-shaped RespFrame.
    #[test]
    fn fr_p2c_002_u012b_resp3_allow_downgrade() {
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        // Default config remains fail-closed.
        assert_eq!(
            parse_frame(b"%1\r\n+name\r\n+alice\r\n").expect_err("fail-closed without allow_resp3"),
            RespParseError::UnsupportedResp3Type(b'%')
        );

        // Map → flat Array of 2N entries.
        let parsed = parse_frame_with_config(b"%2\r\n+name\r\n+alice\r\n+age\r\n:30\r\n", &allow)
            .expect("map parses under allow_resp3");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("name".to_string()),
                RespFrame::SimpleString("alice".to_string()),
                RespFrame::SimpleString("age".to_string()),
                RespFrame::Integer(30),
            ]))
        );

        // Set → Array.
        let parsed = parse_frame_with_config(b"~2\r\n+a\r\n+b\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("a".to_string()),
                RespFrame::SimpleString("b".to_string()),
            ]))
        );

        // Push → Array.
        let parsed = parse_frame_with_config(b">2\r\n+message\r\n+payload\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("message".to_string()),
                RespFrame::SimpleString("payload".to_string()),
            ]))
        );

        // Bool → Integer 0/1.
        let parsed = parse_frame_with_config(b"#t\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Integer(1));
        let parsed = parse_frame_with_config(b"#f\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Integer(0));

        // Null → BulkString(None).
        let parsed = parse_frame_with_config(b"_\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::BulkString(None));

        // Double → BulkString of the ASCII payload.
        let parsed = parse_frame_with_config(b",3.14\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::BulkString(Some(b"3.14".to_vec())));

        // Verbatim → BulkString with 4-byte "txt:" prefix stripped.
        let parsed = parse_frame_with_config(b"=15\r\ntxt:hello world\r\n", &allow).unwrap();
        assert_eq!(
            parsed.frame,
            RespFrame::BulkString(Some(b"hello world".to_vec()))
        );

        // Attribute: peel the attribute map, return the next frame.
        let parsed = parse_frame_with_config(b"|1\r\n+meta\r\n+value\r\n+OK\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::SimpleString("OK".to_string()));

        // Blob error → Error.
        let parsed = parse_frame_with_config(b"!5\r\nWRONG\r\n", &allow).unwrap();
        assert_eq!(parsed.frame, RespFrame::Error("WRONG".to_string()));
    }

    #[test]
    fn resp3_null_rejects_non_empty_payload() {
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        assert_eq!(
            parse_frame_with_config(b"_payload\r\n", &allow),
            Err(RespParseError::InvalidBulkLength)
        );
        assert_eq!(
            parse_frame_with_config(b"_0\r\n", &allow),
            Err(RespParseError::InvalidBulkLength)
        );
        assert_eq!(
            parse_frame_with_config(b"_\r\n", &allow)
                .expect("canonical RESP3 null must parse")
                .frame,
            RespFrame::BulkString(None)
        );
    }

    #[test]
    fn format_redis_double_matches_vendored_d2string() {
        // Golden values captured from vendored redis 7.2.4 (util.c::d2string
        // -> fpconv_dtoa). Verified byte-exact against the oracle across 437
        // diverse magnitudes; these lock the representative branches and keep
        // the RESP3 Double path in lockstep with RESP2. (frankenredis-sk4ss)
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "-0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (3.0, "3"),
            (17179869184.0, "17179869184"),
            (-42.0, "-42"),
            (2.75, "2.75"),
            (123.456, "123.456"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (0.000001, "0.000001"),
            (-0.0007, "-0.0007"),
            (123_456_789.123_456_79, "1.2345678912345679e+8"),
            (1.5e300, "1.5e+300"),
            (1e20, "1e+20"),
            (1e308, "1e+308"),
            (-1.5e300, "-1.5e+300"),
            (6.022e23, "6.022e+23"),
            (1e-7, "1e-7"),
            (1e-10, "1e-10"),
            (1.6e-19, "1.6e-19"),
            // (frankenredis fpconv grisu2 port) Cases the prior Ryū-piggyback
            // diverged on — large integer-valued doubles where grisu2 shortens
            // the exact integer, the 17-sig-digit grisu2 last-digit tie-break,
            // and the ±2^62 double2ll window edges. Captured from redis 7.2.4.
            (1234567890123456.7, "1234567890123456.7"),
            (-1997107851181081.2, "-1997107851181081.2"),
            (6300820258065050624.0, "6300820258065051000"),
            (1373428634809579008.0, "1373428634809579008"),
            (9223372036854774784.0, "9223372036854775000"),
            (4611686018427387904.0, "4611686018427387904"),
            (9223372036854775807.0, "9223372036854776000"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                &format_redis_double(*value),
                expected,
                "format_redis_double({value:?})"
            );
            assert_eq!(
                RespFrame::double_from_f64(*value),
                RespFrame::Double((*expected).to_string()),
                "double_from_f64({value:?})"
            );
        }
        assert_eq!(format_redis_double(f64::NAN), "nan");
    }

    #[test]
    fn direct_redis_double_encoding_matches_existing_frames() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "-0"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (3.0, "3"),
            (-42.0, "-42"),
            (2.75, "2.75"),
            (123.456, "123.456"),
            (1e20, "1e+20"),
            (1e-10, "1e-10"),
            (6300820258065050624.0, "6300820258065051000"),
            (9223372036854775807.0, "9223372036854776000"),
        ];

        for (value, expected) in cases {
            let mut ascii = Vec::new();
            push_redis_double_ascii(&mut ascii, *value);
            assert_eq!(ascii, expected.as_bytes(), "ascii {value:?}");

            let mut resp2 = Vec::new();
            encode_redis_double(*value, false, &mut resp2);
            assert_eq!(
                resp2,
                RespFrame::BulkString(Some(expected.as_bytes().to_vec())).to_bytes(),
                "resp2 {value:?}"
            );

            let mut resp3 = Vec::new();
            encode_redis_double(*value, true, &mut resp3);
            assert_eq!(
                resp3,
                RespFrame::Double((*expected).to_string()).to_bytes(),
                "resp3 {value:?}"
            );
        }
    }

    #[test]
    fn resp3_double_rejects_empty_and_non_numeric_payloads() {
        // (frankenredis-u1xg5) Continuing ny5fu's RESP3 strictness
        // pass for the ',' (double) prefix. Empty / garbage payloads
        // are malformed and must be rejected with InvalidInteger
        // instead of silently passing as BulkString(Some(...)).
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_frame_with_config(b",\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "empty double must reject"
        );
        assert_eq!(
            parse_frame_with_config(b",abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-numeric double must reject"
        );
        assert_eq!(
            parse_frame_with_config(b",1.2.3\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "double-dotted double must reject"
        );

        // Canonical doubles still parse to BulkString of the payload.
        for ok in [
            &b",3.14\r\n"[..],
            &b",-0.5\r\n"[..],
            &b",1e10\r\n"[..],
            &b",inf\r\n"[..],
            &b",-inf\r\n"[..],
            &b",nan\r\n"[..],
        ] {
            assert!(
                matches!(
                    parse_frame_with_config(ok, &allow),
                    Ok(parsed) if matches!(parsed.frame, RespFrame::BulkString(Some(_)))
                ),
                "canonical double {ok:?} should parse"
            );
        }
    }

    #[test]
    fn resp3_big_number_rejects_empty_and_non_numeric_payloads() {
        // (frankenredis-u1xg5) Same strictness for the '(' (big
        // number) prefix. Body must be all decimal digits with an
        // optional leading +/-.
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };
        assert_eq!(
            parse_frame_with_config(b"(\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "empty big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-numeric big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(+\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "sign-only big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(1.5\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "non-integer big number must reject"
        );
        assert_eq!(
            parse_frame_with_config(b"(123abc\r\n", &allow),
            Err(RespParseError::InvalidInteger),
            "trailing-garbage big number must reject"
        );

        // Canonical big numbers still parse.
        for ok in [
            &b"(0\r\n"[..],
            &b"(12345\r\n"[..],
            &b"(-42\r\n"[..],
            &b"(+99999999999999999999\r\n"[..],
        ] {
            assert!(
                matches!(
                    parse_frame_with_config(ok, &allow),
                    Ok(parsed) if matches!(parsed.frame, RespFrame::BulkString(Some(_)))
                ),
                "canonical big number {ok:?} should parse"
            );
        }
    }

    #[test]
    fn resp3_verbatim_rejects_missing_format_prefix() -> Result<(), String> {
        // (frankenredis-gg805) RESP3 verbatim bodies are
        // `<3-byte-format>:<payload>`. Accepting bodies without the
        // mandatory format prefix lets malformed replies pass through
        // as ordinary bulk strings once allow_resp3 is enabled.
        let allow = ParserConfig {
            allow_resp3: true,
            ..ParserConfig::default()
        };

        for (input, context) in [
            (
                &b"=3\r\nabc\r\n"[..],
                "verbatim body shorter than the format prefix",
            ),
            (&b"=4\r\nabcd\r\n"[..], "verbatim body with no prefix colon"),
            (
                &b"=0\r\n\r\n"[..],
                "empty verbatim body cannot carry the mandatory format prefix",
            ),
        ] {
            let actual = parse_frame_with_config(input, &allow);
            if actual != Err(RespParseError::InvalidBulkLength) {
                return Err(format!("{context} should reject; got {actual:?}"));
            }
        }

        let parsed = parse_frame_with_config(b"=4\r\ntxt:\r\n", &allow)
            .map_err(|err| format!("empty verbatim payload with prefix should parse: {err:?}"))?;
        if parsed.frame != RespFrame::BulkString(Some(Vec::new())) {
            return Err(format!(
                "empty verbatim payload stripped incorrectly: {parsed:?}"
            ));
        }
        Ok(())
    }

    #[test]
    fn resp3_attribute_chain_is_capped_independently_of_recursion_depth() {
        // (frankenredis-oafun) The xfxtd fix made the attribute branch
        // depth-transparent, so without a separate chain cap a
        // malicious peer could send '|1\r\n+k\r\n+v\r\n' × N to grow
        // the parser's Rust call stack linearly at the same depth.
        // RESP3_ATTRIBUTE_CHAIN_LIMIT (8) catches that. Generate a
        // 16-deep chain wrapping a single SimpleString and assert
        // the parser rejects it cleanly with RecursionLimitExceeded
        // — well before any real workload would trip the cap.
        let mut payload = Vec::new();
        for _ in 0..16 {
            payload.extend_from_slice(b"|1\r\n+k\r\n+v\r\n");
        }
        payload.extend_from_slice(b"+OK\r\n");

        let cfg = ParserConfig {
            // Very generous depth budget to isolate the chain cap as
            // the actual rejection signal.
            max_recursion_depth: 1024,
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let result = parse_frame_with_config(&payload, &cfg);
        assert_eq!(result, Err(RespParseError::RecursionLimitExceeded));
    }

    #[test]
    fn resp3_attribute_chain_under_cap_still_parses() {
        // (frankenredis-oafun) 7 chained attributes (well under the
        // cap of 8) must still resolve to the wrapped frame. Pins
        // that the cap doesn't accidentally reject legitimately
        // multi-attribute replies.
        let mut payload = Vec::new();
        for _ in 0..7 {
            payload.extend_from_slice(b"|1\r\n+k\r\n+v\r\n");
        }
        payload.extend_from_slice(b"+OK\r\n");

        let cfg = ParserConfig {
            max_recursion_depth: 1024,
            allow_resp3: true,
            ..ParserConfig::default()
        };
        let parsed = parse_frame_with_config(&payload, &cfg).expect("7 attrs must parse");
        assert_eq!(parsed.frame, RespFrame::SimpleString("OK".to_string()));
    }

    #[test]
    fn resp3_attribute_wrapper_does_not_spend_extra_recursion_depth() {
        let allow_tight_depth = ParserConfig {
            max_recursion_depth: 1,
            allow_resp3: true,
            ..ParserConfig::default()
        };

        let parsed = parse_frame_with_config(
            b"|1\r\n+meta\r\n+value\r\n*1\r\n+OK\r\n",
            &allow_tight_depth,
        )
        .expect("attribute metadata must not consume the wrapped frame's depth budget");
        assert_eq!(
            parsed.frame,
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())]))
        );
    }

    #[test]
    fn fr_p2c_002_u013_bulk_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_bulk_len: 5,
            ..ParserConfig::default()
        };
        let accepted =
            parse_frame_with_config(b"$5\r\nhello\r\n", &config).expect("bulk at limit parses");
        assert_eq!(
            accepted.frame,
            RespFrame::BulkString(Some(b"hello".to_vec()))
        );
        assert_eq!(accepted.consumed, b"$5\r\nhello\r\n".len());

        let rejected = parse_frame_with_config(b"$6\r\nhello!\r\n", &config)
            .expect_err("bulk above limit must fail");
        assert_eq!(rejected, RespParseError::BulkLengthTooLarge);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"$5\r\nhello\r\n");
        input_acc.extend_from_slice(b"$6\r\nhello!\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u013_bulk_limit_clamp_holds_at_boundary",
            "unit",
            67,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.bulk_length_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u014_array_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_array_len: 1,
            ..ParserConfig::default()
        };
        let accepted =
            parse_frame_with_config(b"*1\r\n+OK\r\n", &config).expect("array at limit parses");
        assert_eq!(
            accepted.frame,
            RespFrame::Array(Some(vec![RespFrame::SimpleString("OK".to_string())]))
        );
        assert_eq!(accepted.consumed, b"*1\r\n+OK\r\n".len());

        let rejected = parse_frame_with_config(b"*2\r\n+OK\r\n+OK\r\n", &config)
            .expect_err("array above limit must fail");
        assert_eq!(rejected, RespParseError::MultibulkLengthTooLarge);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(b"*1\r\n+OK\r\n");
        input_acc.extend_from_slice(b"*2\r\n+OK\r\n+OK\r\n");

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u014_array_limit_clamp_holds_at_boundary",
            "unit",
            71,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.array_length_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn fr_p2c_002_u015_recursion_limit_clamp_holds_at_boundary() {
        let config = ParserConfig {
            max_recursion_depth: 2,
            ..ParserConfig::default()
        };
        let accepted_frame = nested_singleton_array(2);
        let accepted_bytes = accepted_frame.to_bytes();
        let accepted = parse_frame_with_config(accepted_bytes.as_slice(), &config)
            .expect("frame at recursion limit parses");
        assert_eq!(accepted.frame, accepted_frame);
        assert_eq!(accepted.consumed, accepted_bytes.len());

        let rejected_frame = nested_singleton_array(3);
        let rejected_bytes = rejected_frame.to_bytes();
        let rejected = parse_frame_with_config(rejected_bytes.as_slice(), &config)
            .expect_err("frame above recursion limit must fail");
        assert_eq!(rejected, RespParseError::RecursionLimitExceeded);

        let mut input_acc = Vec::new();
        input_acc.extend_from_slice(accepted_bytes.as_slice());
        input_acc.extend_from_slice(rejected_bytes.as_slice());

        let mut output_acc = Vec::new();
        output_acc.extend_from_slice(accepted.frame.to_bytes().as_slice());
        output_acc.extend_from_slice(rejected.to_string().as_bytes());

        let event = build_event(
            "fr_p2c_002_u015_recursion_limit_clamp_holds_at_boundary",
            "unit",
            73,
            input_acc.as_slice(),
            output_acc.as_slice(),
            "protocol.recursion_limit_clamp_enforced",
        );
        event.assert_schema_contract();
    }

    #[test]
    fn sequence_frames_encode_as_back_to_back_resp_messages() {
        let frame = RespFrame::Sequence(vec![
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"ch1".to_vec())),
                RespFrame::Integer(1),
            ])),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"subscribe".to_vec())),
                RespFrame::BulkString(Some(b"ch2".to_vec())),
                RespFrame::Integer(2),
            ])),
        ]);

        assert_eq!(
            frame.to_bytes(),
            b"*3\r\n$9\r\nsubscribe\r\n$3\r\nch1\r\n:1\r\n*3\r\n$9\r\nsubscribe\r\n$3\r\nch2\r\n:2\r\n"
                .to_vec()
        );
    }

    #[test]
    fn resp3_map_frames_encode_with_percent_prefix() {
        let frame = RespFrame::Map(Some(vec![
            (
                RespFrame::BulkString(Some(b"server".to_vec())),
                RespFrame::BulkString(Some(b"redis".to_vec())),
            ),
            (
                RespFrame::BulkString(Some(b"proto".to_vec())),
                RespFrame::Integer(3),
            ),
        ]));

        assert_eq!(
            frame.to_bytes(),
            b"%2\r\n$6\r\nserver\r\n$5\r\nredis\r\n$5\r\nproto\r\n:3\r\n".to_vec()
        );
    }

    #[test]
    fn resp3_push_frames_encode_with_angle_prefix() {
        let frame = RespFrame::Push(vec![
            RespFrame::BulkString(Some(b"message".to_vec())),
            RespFrame::BulkString(Some(b"channel".to_vec())),
            RespFrame::BulkString(Some(b"payload".to_vec())),
        ]);

        assert_eq!(
            frame.to_bytes(),
            b">3\r\n$7\r\nmessage\r\n$7\r\nchannel\r\n$7\r\npayload\r\n".to_vec()
        );
    }

    // ── Proptest fuzz tests ──────────────────────────────────────────

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(10_000))]

            #[test]
            fn parse_frame_never_panics(data: Vec<u8>) {
                let _ = parse_frame(&data);
            }

            #[test]
            fn parse_frame_with_config_never_panics(data: Vec<u8>) {
                let config = ParserConfig::default();
                let _ = parse_frame_with_config(&data, &config);
            }

            #[test]
            fn parse_frame_with_tight_limits_never_panics(data: Vec<u8>) {
                let config = ParserConfig {
                    max_bulk_len: 64,
                    max_array_len: 4,
                    max_recursion_depth: 2,
                    ..ParserConfig::default()
                };
                let _ = parse_frame_with_config(&data, &config);
            }

            #[test]
            fn parse_frame_with_resp_prefix_never_panics(
                prefix in prop::sample::select(vec![b'+', b'-', b':', b'$', b'*']),
                payload: Vec<u8>,
            ) {
                let mut data = vec![prefix];
                data.extend_from_slice(&payload);
                let _ = parse_frame(&data);
            }
        }
    }

    /// Golden artifact tests: verify RESP encoding produces exact expected bytes.
    /// These catch accidental encoding format changes that would break wire compatibility.
    mod golden {
        use super::*;

        /// Golden test: SimpleString encoding must produce exact bytes.
        #[test]
        fn golden_simple_string_ok() {
            let frame = RespFrame::SimpleString("OK".to_string());
            let golden = b"+OK\r\n";
            assert_eq!(frame.to_bytes(), golden, "SimpleString encoding changed");
        }

        /// Golden test: SimpleString with spaces and special chars.
        #[test]
        fn golden_simple_string_pong() {
            let frame = RespFrame::SimpleString("PONG".to_string());
            let golden = b"+PONG\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "SimpleString PONG encoding changed"
            );
        }

        /// Golden test: Error encoding must produce exact bytes.
        #[test]
        fn golden_error_generic() {
            let frame = RespFrame::Error("ERR unknown command".to_string());
            let golden = b"-ERR unknown command\r\n";
            assert_eq!(frame.to_bytes(), golden, "Error encoding changed");
        }

        /// Golden test: Error with WRONGTYPE prefix.
        #[test]
        fn golden_error_wrongtype() {
            let frame = RespFrame::Error(
                "WRONGTYPE Operation against a key holding the wrong kind of value".to_string(),
            );
            let golden = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
            assert_eq!(frame.to_bytes(), golden, "WRONGTYPE error encoding changed");
        }

        /// Golden test: positive integer encoding.
        #[test]
        fn golden_integer_positive() {
            let frame = RespFrame::Integer(42);
            let golden = b":42\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Positive integer encoding changed"
            );
        }

        /// Golden test: negative integer encoding.
        #[test]
        fn golden_integer_negative() {
            let frame = RespFrame::Integer(-1);
            let golden = b":-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Negative integer encoding changed"
            );
        }

        /// Golden test: zero integer encoding.
        #[test]
        fn golden_integer_zero() {
            let frame = RespFrame::Integer(0);
            let golden = b":0\r\n";
            assert_eq!(frame.to_bytes(), golden, "Zero integer encoding changed");
        }

        /// Golden test: large integer encoding (Redis INCR max).
        #[test]
        fn golden_integer_large() {
            let frame = RespFrame::Integer(9_223_372_036_854_775_807);
            let golden = b":9223372036854775807\r\n";
            assert_eq!(frame.to_bytes(), golden, "Large integer encoding changed");
        }

        /// Golden test: null bulk string encoding.
        #[test]
        fn golden_bulk_null() {
            let frame = RespFrame::BulkString(None);
            let golden = b"$-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Null bulk string encoding changed"
            );
        }

        /// Golden test: empty bulk string encoding.
        #[test]
        fn golden_bulk_empty() {
            let frame = RespFrame::BulkString(Some(vec![]));
            let golden = b"$0\r\n\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Empty bulk string encoding changed"
            );
        }

        /// Golden test: simple bulk string encoding.
        #[test]
        fn golden_bulk_hello() {
            let frame = RespFrame::BulkString(Some(b"hello".to_vec()));
            let golden = b"$5\r\nhello\r\n";
            assert_eq!(frame.to_bytes(), golden, "Bulk string encoding changed");
        }

        /// Golden test: bulk string with binary data (including null bytes).
        #[test]
        fn golden_bulk_binary() {
            let frame = RespFrame::BulkString(Some(vec![0x00, 0xFF, 0x0D, 0x0A]));
            let golden = b"$4\r\n\x00\xFF\x0D\x0A\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Binary bulk string encoding changed"
            );
        }

        #[test]
        fn borrowed_bulk_slice_encoder_matches_frame_encoder() {
            let payload = b"hello\r\nbinary\0";

            let mut borrowed_resp2 = Vec::new();
            crate::encode_bulk_string_slice(Some(payload), false, &mut borrowed_resp2);
            assert_eq!(
                borrowed_resp2,
                RespFrame::BulkString(Some(payload.to_vec())).to_bytes()
            );

            let mut null_resp2 = Vec::new();
            crate::encode_bulk_string_slice(None, false, &mut null_resp2);
            assert_eq!(null_resp2, RespFrame::BulkString(None).to_bytes());

            let mut null_resp3 = Vec::new();
            crate::encode_bulk_string_slice(None, true, &mut null_resp3);
            let mut frame_resp3 = Vec::new();
            RespFrame::BulkString(None).encode_into_resp3(&mut frame_resp3);
            assert_eq!(null_resp3, frame_resp3);
        }

        #[test]
        fn to_bytes_capacity_hint_preserves_encode_into_bytes() {
            let frames = vec![
                RespFrame::SimpleString("OK\r\nstill-one-frame".to_string()),
                RespFrame::Error("ERR sample".to_string()),
                RespFrame::Integer(i64::MIN),
                RespFrame::BulkString(None),
                RespFrame::BulkString(Some(vec![0x00, 0xFF, b'\r', b'\n'])),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
                RespFrame::Map(Some(vec![(
                    RespFrame::SimpleString("key".to_string()),
                    RespFrame::Integer(7),
                )])),
                RespFrame::Push(vec![
                    RespFrame::SimpleString("message".to_string()),
                    RespFrame::BulkString(Some(b"payload".to_vec())),
                ]),
                RespFrame::Sequence(vec![RespFrame::Integer(1), RespFrame::Integer(2)]),
                RespFrame::Double("-inf".to_string()),
                RespFrame::Set(None),
                RespFrame::Set(Some(vec![RespFrame::BulkString(Some(b"member".to_vec()))])),
                RespFrame::Verbatim("hello world".to_string()),
            ];

            for frame in frames {
                let mut encode_into_bytes = Vec::new();
                frame.encode_into(&mut encode_into_bytes);
                let to_bytes = frame.to_bytes();
                assert_eq!(to_bytes, encode_into_bytes);
                assert_eq!(frame.encoded_len_hint(), Some(to_bytes.len()));
            }
        }

        /// Golden test: null array encoding.
        #[test]
        fn golden_array_null() {
            let frame = RespFrame::Array(None);
            let golden = b"*-1\r\n";
            assert_eq!(frame.to_bytes(), golden, "Null array encoding changed");
        }

        /// Golden test: empty array encoding.
        #[test]
        fn golden_array_empty() {
            let frame = RespFrame::Array(Some(vec![]));
            let golden = b"*0\r\n";
            assert_eq!(frame.to_bytes(), golden, "Empty array encoding changed");
        }

        /// Golden test: array with single integer.
        #[test]
        fn golden_array_single_int() {
            let frame = RespFrame::Array(Some(vec![RespFrame::Integer(1)]));
            let golden = b"*1\r\n:1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "Single-element array encoding changed"
            );
        }

        /// Golden test: array with mixed types (typical LRANGE response).
        #[test]
        fn golden_array_mixed() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"first".to_vec())),
                RespFrame::BulkString(Some(b"second".to_vec())),
            ]));
            let golden = b"*2\r\n$5\r\nfirst\r\n$6\r\nsecond\r\n";
            assert_eq!(frame.to_bytes(), golden, "Mixed array encoding changed");
        }

        /// Golden test: nested array (typical XREAD response structure).
        #[test]
        fn golden_array_nested() {
            let frame = RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::Integer(1),
                RespFrame::Integer(2),
            ]))]));
            let golden = b"*1\r\n*2\r\n:1\r\n:2\r\n";
            assert_eq!(frame.to_bytes(), golden, "Nested array encoding changed");
        }

        /// Golden test: typical SET command (client request format).
        #[test]
        fn golden_command_set() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"key".to_vec())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ]));
            let golden = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
            assert_eq!(frame.to_bytes(), golden, "SET command encoding changed");
        }

        /// Golden test: typical GET response (null for missing key).
        #[test]
        fn golden_response_get_miss() {
            let frame = RespFrame::BulkString(None);
            let golden = b"$-1\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "GET miss response encoding changed"
            );
        }

        /// Golden test: SCAN response format (cursor + keys array).
        #[test]
        fn golden_response_scan() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"key1".to_vec())),
                    RespFrame::BulkString(Some(b"key2".to_vec())),
                ])),
            ]));
            let golden = b"*2\r\n$1\r\n0\r\n*2\r\n$4\r\nkey1\r\n$4\r\nkey2\r\n";
            assert_eq!(frame.to_bytes(), golden, "SCAN response encoding changed");
        }

        /// Golden test: HGETALL response format (field-value pairs).
        #[test]
        fn golden_response_hgetall() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"field1".to_vec())),
                RespFrame::BulkString(Some(b"value1".to_vec())),
                RespFrame::BulkString(Some(b"field2".to_vec())),
                RespFrame::BulkString(Some(b"value2".to_vec())),
            ]));
            let golden = b"*4\r\n$6\r\nfield1\r\n$6\r\nvalue1\r\n$6\r\nfield2\r\n$6\r\nvalue2\r\n";
            assert_eq!(
                frame.to_bytes(),
                golden,
                "HGETALL response encoding changed"
            );
        }

        /// Golden test: BLPOP response format (key + value).
        #[test]
        fn golden_response_blpop() {
            let frame = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::BulkString(Some(b"element".to_vec())),
            ]));
            let golden = b"*2\r\n$6\r\nmylist\r\n$7\r\nelement\r\n";
            assert_eq!(frame.to_bytes(), golden, "BLPOP response encoding changed");
        }

        /// Golden test: Sequence frame encoding (multiple frames concatenated).
        #[test]
        fn golden_sequence() {
            let frame = RespFrame::Sequence(vec![
                RespFrame::SimpleString("OK".to_string()),
                RespFrame::Integer(1),
            ]);
            let golden = b"+OK\r\n:1\r\n";
            assert_eq!(frame.to_bytes(), golden, "Sequence encoding changed");
        }
    }

    /// Metamorphic tests for RESP encoding/decoding invariants.
    mod metamorphic {
        use super::super::{RespFrame, parse_frame};
        use proptest::prelude::*;

        fn arb_simple_string() -> impl Strategy<Value = RespFrame> {
            "[a-zA-Z0-9 ]{0,50}"
                .prop_filter("no CRLF", |s| !s.contains('\r') && !s.contains('\n'))
                .prop_map(RespFrame::SimpleString)
        }

        fn arb_error() -> impl Strategy<Value = RespFrame> {
            "[A-Z]{3,10} [a-zA-Z0-9 ]{0,40}"
                .prop_filter("no CRLF", |s| !s.contains('\r') && !s.contains('\n'))
                .prop_map(RespFrame::Error)
        }

        fn arb_integer() -> impl Strategy<Value = RespFrame> {
            any::<i64>().prop_map(RespFrame::Integer)
        }

        fn arb_bulk_string() -> impl Strategy<Value = RespFrame> {
            prop_oneof![
                Just(RespFrame::BulkString(None)),
                prop::collection::vec(any::<u8>(), 0..100)
                    .prop_map(|v| RespFrame::BulkString(Some(v))),
            ]
        }

        fn arb_frame_leaf() -> impl Strategy<Value = RespFrame> {
            prop_oneof![
                arb_simple_string(),
                arb_error(),
                arb_integer(),
                arb_bulk_string(),
            ]
        }

        fn arb_frame() -> impl Strategy<Value = RespFrame> {
            arb_frame_leaf().prop_recursive(3, 32, 8, |inner| {
                prop_oneof![
                    Just(RespFrame::Array(None)),
                    prop::collection::vec(inner.clone(), 0..8)
                        .prop_map(|v| RespFrame::Array(Some(v))),
                ]
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]

            /// MR1: Encode-decode roundtrip identity
            /// encode(frame) → parse(encoded) == frame
            #[test]
            fn mr_encode_decode_roundtrip(frame in arb_frame()) {
                let encoded = frame.to_bytes();
                let parsed = parse_frame(&encoded).expect("encoded frame must parse");
                prop_assert_eq!(parsed.frame, frame, "roundtrip mismatch");
                prop_assert_eq!(parsed.consumed, encoded.len(), "consumed mismatch");
            }

            /// MR2: Encoding determinism
            /// encode(frame) == encode(clone(frame))
            #[test]
            fn mr_encoding_determinism(frame in arb_frame()) {
                let enc1 = frame.to_bytes();
                let enc2 = frame.clone().to_bytes();
                prop_assert_eq!(enc1, enc2, "encoding not deterministic");
            }

            /// MR3: Encoding length monotonicity for bulk strings
            /// len(encode(bulk(a))) < len(encode(bulk(a ++ b))) when b is non-empty
            #[test]
            fn mr_bulk_length_monotonic(
                base in prop::collection::vec(any::<u8>(), 0..50),
                extra in prop::collection::vec(any::<u8>(), 1..20),
            ) {
                let short_frame = RespFrame::BulkString(Some(base.clone()));
                let mut long_data = base.clone();
                long_data.extend(&extra);
                let long_frame = RespFrame::BulkString(Some(long_data));

                let short_enc = short_frame.to_bytes();
                let long_enc = long_frame.to_bytes();

                prop_assert!(short_enc.len() < long_enc.len(),
                    "adding bytes should increase encoding length: {} vs {}",
                    short_enc.len(), long_enc.len());
            }

            /// MR4: Array length encoding correctness
            /// len(array) == count from encoded header
            #[test]
            fn mr_array_length_encoding(elements in prop::collection::vec(arb_frame_leaf(), 0..20)) {
                let frame = RespFrame::Array(Some(elements.clone()));
                let encoded = frame.to_bytes();

                // Parse the array count from the header
                let header_end = encoded.windows(2)
                    .position(|w| w == b"\r\n")
                    .expect("must have CRLF");
                let count_str = std::str::from_utf8(&encoded[1..header_end])
                    .expect("count must be ASCII");
                let count: usize = count_str.parse().expect("count must be number");

                prop_assert_eq!(count, elements.len(),
                    "array count in header doesn't match element count");
            }

            /// MR5: Concatenated encoding equals sequence encoding
            /// concat(encode(a), encode(b)) == encode(Sequence([a, b]))
            #[test]
            fn mr_sequence_concat_equivalence(
                frame_a in arb_frame_leaf(),
                frame_b in arb_frame_leaf(),
            ) {
                let concat = {
                    let mut v = frame_a.to_bytes();
                    v.extend(frame_b.to_bytes());
                    v
                };
                let seq = RespFrame::Sequence(vec![frame_a.clone(), frame_b.clone()]);
                let seq_encoded = seq.to_bytes();

                prop_assert_eq!(concat, seq_encoded,
                    "sequence encoding differs from concatenation");
            }

            /// MR6: Integer encoding preserves ordering
            /// a < b => encode(:a) lexicographically relates to encode(:b)
            /// (Note: not lex order due to length prefixes, but decoded value order)
            #[test]
            fn mr_integer_order_preservation(a in -10000i64..10000i64, b in -10000i64..10000i64) {
                let frame_a = RespFrame::Integer(a);
                let frame_b = RespFrame::Integer(b);

                let enc_a = frame_a.to_bytes();
                let enc_b = frame_b.to_bytes();

                let parsed_a = parse_frame(&enc_a).expect("must parse").frame;
                let parsed_b = parse_frame(&enc_b).expect("must parse").frame;

                if let (RespFrame::Integer(va), RespFrame::Integer(vb)) = (parsed_a, parsed_b) {
                    if a < b {
                        prop_assert!(va < vb, "order not preserved: {} < {} but {} >= {}", a, b, va, vb);
                    } else if a > b {
                        prop_assert!(va > vb, "order not preserved: {} > {} but {} <= {}", a, b, va, vb);
                    } else {
                        prop_assert_eq!(va, vb, "equal integers should decode equal");
                    }
                } else {
                    prop_assert!(false, "decoded frames are not integers");
                }
            }

            /// MR7: Nested arrays decode to matching depth
            #[test]
            fn mr_nested_array_depth(depth in 1usize..6, value in any::<i64>()) {
                // Build nested array: [[[[value]]]] with `depth` levels
                let mut frame = RespFrame::Integer(value);
                for _ in 0..depth {
                    frame = RespFrame::Array(Some(vec![frame]));
                }

                let encoded = frame.to_bytes();
                let parsed = parse_frame(&encoded).expect("nested array must parse");

                // Unwrap the nesting to verify depth
                let mut current = parsed.frame;
                let mut actual_depth = 0;
                while let RespFrame::Array(Some(inner)) = current {
                    actual_depth += 1;
                    current = inner.into_iter().next().expect("should have one element");
                }

                prop_assert_eq!(actual_depth, depth, "nesting depth mismatch");

                // Verify the inner value
                if let RespFrame::Integer(v) = current {
                    prop_assert_eq!(v, value, "inner value mismatch");
                } else {
                    prop_assert!(false, "inner value is not an integer");
                }
            }
        }
    }
}
