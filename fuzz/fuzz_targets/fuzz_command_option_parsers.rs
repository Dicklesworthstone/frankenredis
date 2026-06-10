#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_command::{CommandError, dispatch_argv};
use fr_protocol::RespFrame;
use fr_store::Store;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_CASES: usize = 64;
const MAX_BLOB_LEN: usize = 32;
const TEXT_SEED_HEADER: &str = "FR_OPTION_SEED";

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    cases: Vec<ParserCase>,
}

#[derive(Debug, Arbitrary)]
enum ParserCase {
    ZRangeByScore(ZRangeByScoreCase),
    GeoSearch(GeoSearchCase),
    Stream(StreamCase),
    ZStore(ZStoreCase),
    Scan(ScanCase),
    Eval(EvalCase),
    BlockingTimeout(BlockingTimeoutCase),
}

#[derive(Debug, Clone, Arbitrary)]
struct Blob(Vec<u8>);

#[derive(Debug, Arbitrary)]
struct ZRangeByScoreCase {
    tokens: Vec<ZRangeToken>,
}

#[derive(Debug, Arbitrary)]
enum ZRangeToken {
    WithScores,
    Limit { offset: i16, count: i16 },
    InvalidKeyword(Blob),
    MissingLimitCount { offset: i16 },
}

#[derive(Debug, Arbitrary)]
struct GeoSearchCase {
    flags: Vec<GeoFlagToken>,
}

#[derive(Debug, Arbitrary)]
enum GeoFlagToken {
    WithCoord,
    WithDist,
    WithHash,
    Count(i16),
    Any,
    Asc,
    Desc,
    InvalidKeyword(Blob),
    MissingCountValue,
}

#[derive(Debug, Arbitrary)]
enum StreamCase {
    Xread {
        id: StreamIdArg,
    },
    Xrange {
        start: StreamBoundArg,
        end: StreamBoundArg,
    },
    XgroupSetId {
        id: StreamIdArg,
    },
}

#[derive(Debug, Clone, Arbitrary)]
enum StreamIdArg {
    Explicit { ms: u16, seq: u8 },
    BareMs(u16),
    Dollar,
    Dash,
    Plus,
    Invalid(Blob),
}

#[derive(Debug, Clone, Arbitrary)]
enum StreamBoundArg {
    Explicit { ms: u16, seq: u8 },
    BareMs(u16),
    Dash,
    Plus,
    Invalid(Blob),
}

#[derive(Debug, Arbitrary)]
struct ZStoreCase {
    tokens: Vec<ZStoreToken>,
}

#[derive(Debug, Arbitrary)]
enum ZStoreToken {
    Weights { left: i8, right: i8 },
    Aggregate(AggregateKind),
    InvalidKeyword(Blob),
    MissingWeight,
    MissingAggregateValue,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum AggregateKind {
    Sum,
    Min,
    Max,
}

#[derive(Debug, Arbitrary)]
struct ScanCase {
    kind: ScanKind,
    tokens: Vec<ScanToken>,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum ScanKind {
    Scan,
    Hscan,
}

#[derive(Debug, Arbitrary)]
enum ScanToken {
    Match(Blob),
    Count(i16),
    Type(TypeName),
    NoValues,
    InvalidKeyword(Blob),
    MissingMatchValue,
    MissingCountValue,
    MissingTypeValue,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum TypeName {
    String,
    Hash,
    List,
    Set,
    Zset,
    Stream,
    Bogus,
}

#[derive(Debug, Arbitrary)]
struct EvalCase {
    numkeys: NumkeysArg,
    payload: Vec<Blob>,
}

#[derive(Debug, Arbitrary)]
enum NumkeysArg {
    Integer(i8),
    Invalid(Blob),
}

#[derive(Debug, Arbitrary)]
struct BlockingTimeoutCase {
    has_source_value: bool,
    timeout: TimeoutArg,
}

#[derive(Debug, Arbitrary)]
enum TimeoutArg {
    Integer(u16),
    Decimal { whole: u8, frac: u8 },
    Scientific(u8),
    Negative(u8),
    Infinity,
    Nan,
    Invalid(Blob),
}

#[derive(Debug, Clone)]
struct ZRangeByScoreModel {
    withscores: bool,
    // offset is signed: ZRANGEBYSCORE's LIMIT parses both offset and count with
    // the plain getLongFromObjectOrReply (no positivity check), so a negative
    // offset is accepted — it simply skips past the end and yields an empty
    // result. count is likewise signed (negative = unlimited).
    limit: Option<(i64, i16)>,
}

#[derive(Debug, Clone)]
struct GeoSearchModel {
    withcoord: bool,
    withdist: bool,
    withhash: bool,
    count: Option<i16>,
    any: bool,
    asc: bool,
}

#[derive(Debug, Clone)]
struct ZStoreModel {
    weights: Option<(i8, i8)>,
    aggregate: Option<AggregateKind>,
}

#[derive(Debug, Clone)]
struct ScanModel {
    pattern: Option<Vec<u8>>,
    count: Option<i16>,
    type_name: Option<TypeName>,
    novalues: bool,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    if data.starts_with(TEXT_SEED_HEADER.as_bytes()) {
        fuzz_text_seed(data);
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = FuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_command_option_parsers(input);
});

fn fuzz_text_seed(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut now_ms = 1_u64;
    for line in text.lines().skip(1).take(MAX_CASES) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The ACCEPT/REJECT prefix is part of the libFuzzer-mutable input and
        // CANNOT be trusted as an oracle: a single-bit mutation can corrupt the
        // argv (e.g. `SCAN ... TYPE string` -> `SCAN ... TYARGVPE string`) while
        // leaving the `ACCEPT` tag intact, manufacturing a contradiction that is
        // a harness artifact, not an fr bug. We therefore strip the tag only to
        // recover the argv and assert the one property that holds for ANY argv
        // regardless of how the seed was mutated: a command that fr rejects must
        // be side-effect free (validation precedes execution in redis, so a
        // rejected command never mutates the keyspace).
        let argv_text = match line.split_once('|') {
            Some((_tag, rest)) => rest,
            None => line,
        };
        let argv: Vec<Vec<u8>> = argv_text
            .split_ascii_whitespace()
            .take(32)
            .map(|token| token.as_bytes().to_vec())
            .collect();
        if argv.is_empty() {
            continue;
        }

        let mut store = seeded_store(now_ms, true);
        let before = store.state_digest();
        let result = dispatch_argv(&argv, &mut store, now_ms);
        if is_rejection(&result) {
            assert_eq!(
                before,
                store.state_digest(),
                "rejected seed command must not mutate store: argv={argv:?}, result={result:?}"
            );
        }
        now_ms = now_ms.saturating_add(7);
    }
}

fn fuzz_command_option_parsers(input: FuzzInput) {
    let mut now_ms = 1_u64;
    for case in input.cases.into_iter().take(MAX_CASES) {
        match case {
            ParserCase::ZRangeByScore(case) => fuzz_zrangebyscore(case, now_ms),
            ParserCase::GeoSearch(case) => fuzz_geosearch(case, now_ms),
            ParserCase::Stream(case) => fuzz_stream_case(case, now_ms),
            ParserCase::ZStore(case) => fuzz_zstore(case, now_ms),
            ParserCase::Scan(case) => fuzz_scan(case, now_ms),
            ParserCase::Eval(case) => fuzz_eval(case, now_ms),
            ParserCase::BlockingTimeout(case) => fuzz_blocking_timeout(case, now_ms),
        }
        now_ms = now_ms.saturating_add(7);
    }
}

fn fuzz_zrangebyscore(case: ZRangeByScoreCase, now_ms: u64) {
    let mut argv = vec![
        b"ZRANGEBYSCORE".to_vec(),
        b"zset".to_vec(),
        b"1".to_vec(),
        b"4".to_vec(),
    ];
    let model = model_zrange_tokens(&case.tokens, &mut argv);
    match model {
        Some(model) => {
            let mut canonical = vec![
                b"ZRANGEBYSCORE".to_vec(),
                b"zset".to_vec(),
                b"1".to_vec(),
                b"4".to_vec(),
            ];
            if model.withscores {
                canonical.push(b"WITHSCORES".to_vec());
            }
            if let Some((offset, count)) = model.limit {
                canonical.push(b"LIMIT".to_vec());
                canonical.push(offset.to_string().into_bytes());
                canonical.push(count.to_string().into_bytes());
            }
            assert_equivalent(argv, canonical, now_ms, false);
        }
        None => {
            let blob_ambiguous = case
                .tokens
                .iter()
                .any(|t| matches!(t, ZRangeToken::InvalidKeyword(_)));
            assert_modeled_invalid(argv, blob_ambiguous, now_ms);
        }
    }
}

fn fuzz_geosearch(case: GeoSearchCase, now_ms: u64) {
    let mut argv = vec![
        b"GEOSEARCH".to_vec(),
        b"geo".to_vec(),
        b"FROMLONLAT".to_vec(),
        b"13.5".to_vec(),
        b"38.1".to_vec(),
        b"BYRADIUS".to_vec(),
        b"200".to_vec(),
        b"km".to_vec(),
    ];
    let model = model_geo_flags(&case.flags, &mut argv);
    match model {
        Some(model) => {
            let mut canonical = vec![
                b"GEOSEARCH".to_vec(),
                b"geo".to_vec(),
                b"FROMLONLAT".to_vec(),
                b"13.5".to_vec(),
                b"38.1".to_vec(),
                b"BYRADIUS".to_vec(),
                b"200".to_vec(),
                b"km".to_vec(),
            ];
            if let Some(count) = model.count {
                canonical.push(b"COUNT".to_vec());
                canonical.push(count.to_string().into_bytes());
                if model.any {
                    canonical.push(b"ANY".to_vec());
                }
            } else if model.any {
                canonical.push(b"ANY".to_vec());
            }
            canonical.push(if model.asc {
                b"ASC".to_vec()
            } else {
                b"DESC".to_vec()
            });
            if model.withcoord {
                canonical.push(b"WITHCOORD".to_vec());
            }
            if model.withdist {
                canonical.push(b"WITHDIST".to_vec());
            }
            if model.withhash {
                canonical.push(b"WITHHASH".to_vec());
            }
            assert_equivalent(argv, canonical, now_ms, false);
        }
        None => {
            let blob_ambiguous = case
                .flags
                .iter()
                .any(|t| matches!(t, GeoFlagToken::InvalidKeyword(_)));
            assert_modeled_invalid(argv, blob_ambiguous, now_ms);
        }
    }
}

fn fuzz_stream_case(case: StreamCase, now_ms: u64) {
    match case {
        StreamCase::Xread { id } => {
            let raw = render_stream_id_arg(&id);
            let argv = vec![
                b"XREAD".to_vec(),
                b"STREAMS".to_vec(),
                b"s".to_vec(),
                raw,
            ];
            // Non-group XREAD parses the id with the STRICT stream-id parser,
            // plus a special case for `$`:
            //   * `$`            -> valid: resolves to the stream's last_id (or
            //                       0-0 when missing); non-blocking XREAD then
            //                       returns nil. No static canonical rewrite.
            //   * explicit/bare  -> valid: `<ms>` is equivalent to `<ms>-0`.
            //   * `-` / `+`      -> rejected: strict parsing treats the interval
            //                       sentinels as invalid ids ("> is XREADGROUP
            //                       only" is a separate path we do not emit).
            //   * Invalid(blob)  -> ambiguous: arbitrary bytes may render a valid
            //                       bare-ms id, so only the inert-on-reject
            //                       invariant can be asserted.
            match &id {
                StreamIdArg::Dollar => assert_accepted(argv, now_ms),
                StreamIdArg::Explicit { .. } | StreamIdArg::BareMs(_) => {
                    let canonical = canonical_xread_id(&id)
                        .expect("explicit and bare-ms ids always canonicalize");
                    let canonical_argv = vec![
                        b"XREAD".to_vec(),
                        b"STREAMS".to_vec(),
                        b"s".to_vec(),
                        canonical,
                    ];
                    assert_equivalent(argv, canonical_argv, now_ms, false);
                }
                StreamIdArg::Dash | StreamIdArg::Plus => assert_rejected(argv, now_ms),
                StreamIdArg::Invalid(_) => assert_inert_on_reject(argv, now_ms),
            }
        }
        StreamCase::Xrange { start, end } => {
            let argv = vec![
                b"XRANGE".to_vec(),
                b"s".to_vec(),
                render_stream_bound_arg(&start),
                render_stream_bound_arg(&end),
            ];
            // XRANGE parses bounds with the NON-strict interval parser:
            // explicit <ms>-<seq>, bare <ms>, `-` (min) and `+` (max) are ALL
            // valid (and start>end just yields an empty reply). The only way to
            // reach the None arm is a StreamBoundArg::Invalid(blob), whose
            // arbitrary bytes may themselves render a valid bound (a number,
            // "-", "+"), so assert only the inert-on-reject invariant.
            match (
                canonical_stream_range_bound(&start, true),
                canonical_stream_range_bound(&end, false),
            ) {
                (Some(start), Some(end)) => {
                    let canonical_argv = vec![b"XRANGE".to_vec(), b"s".to_vec(), start, end];
                    assert_equivalent(argv, canonical_argv, now_ms, false);
                }
                _ => assert_inert_on_reject(argv, now_ms),
            }
        }
        StreamCase::XgroupSetId { id } => {
            let argv = vec![
                b"XGROUP".to_vec(),
                b"SETID".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                render_stream_id_arg(&id),
            ];
            // XGROUP SETID parses the id with the NON-strict parser
            // (streamParseIDOrReply) and special-cases `$`. The group "g" and
            // stream "s" always exist in seeded_store, so every valid id form
            // returns OK:
            //   * `$`            -> stream last_id
            //   * explicit/bare  -> <ms>-<seq> / <ms>-0
            //   * `-` / `+`      -> 0-0 / max (non-strict accepts the interval
            //                       sentinels, unlike XREAD's strict parser)
            // Only a genuinely unparseable id is rejected, but Invalid(blob) is
            // arbitrary and may render a valid id, so it gets the inert-on-reject
            // invariant rather than an unconditional rejection assertion.
            match id {
                StreamIdArg::Explicit { .. }
                | StreamIdArg::BareMs(_)
                | StreamIdArg::Dollar
                | StreamIdArg::Dash
                | StreamIdArg::Plus => assert_accepted(argv, now_ms),
                StreamIdArg::Invalid(_) => assert_inert_on_reject(argv, now_ms),
            }
        }
    }
}

fn fuzz_zstore(case: ZStoreCase, now_ms: u64) {
    let mut argv = vec![
        b"ZUNIONSTORE".to_vec(),
        b"zdest".to_vec(),
        b"2".to_vec(),
        b"zs1".to_vec(),
        b"zs2".to_vec(),
    ];
    let model = model_zstore_tokens(&case.tokens, &mut argv);
    match model {
        Some(model) => {
            let mut canonical = vec![
                b"ZUNIONSTORE".to_vec(),
                b"zdest".to_vec(),
                b"2".to_vec(),
                b"zs1".to_vec(),
                b"zs2".to_vec(),
            ];
            if let Some((left, right)) = model.weights {
                canonical.push(b"WEIGHTS".to_vec());
                canonical.push(left.to_string().into_bytes());
                canonical.push(right.to_string().into_bytes());
            }
            if let Some(aggregate) = model.aggregate {
                canonical.push(b"AGGREGATE".to_vec());
                canonical.push(aggregate.as_bytes().to_vec());
            }
            assert_equivalent(argv, canonical, now_ms, true);
        }
        None => {
            let blob_ambiguous = case
                .tokens
                .iter()
                .any(|t| matches!(t, ZStoreToken::InvalidKeyword(_)));
            assert_modeled_invalid(argv, blob_ambiguous, now_ms);
        }
    }
}

fn fuzz_scan(case: ScanCase, now_ms: u64) {
    let mut argv = match case.kind {
        ScanKind::Scan => vec![b"SCAN".to_vec(), b"0".to_vec()],
        ScanKind::Hscan => vec![b"HSCAN".to_vec(), b"h".to_vec(), b"0".to_vec()],
    };
    let model = model_scan_tokens(&case.tokens, &mut argv);
    match model {
        Some(model) => {
            let mut canonical = match case.kind {
                ScanKind::Scan => vec![b"SCAN".to_vec(), b"0".to_vec()],
                ScanKind::Hscan => vec![b"HSCAN".to_vec(), b"h".to_vec(), b"0".to_vec()],
            };
            if let Some(pattern) = model.pattern {
                canonical.push(b"MATCH".to_vec());
                canonical.push(pattern);
            }
            if let Some(count) = model.count {
                canonical.push(b"COUNT".to_vec());
                canonical.push(count.to_string().into_bytes());
            }
            if let Some(type_name) = model.type_name {
                canonical.push(b"TYPE".to_vec());
                canonical.push(type_name.as_bytes().to_vec());
            }
            if model.novalues {
                canonical.push(b"NOVALUES".to_vec());
            }
            assert_equivalent(argv, canonical, now_ms, false);
        }
        None => {
            let blob_ambiguous = case
                .tokens
                .iter()
                .any(|t| matches!(t, ScanToken::InvalidKeyword(_)));
            assert_modeled_invalid(argv, blob_ambiguous, now_ms);
        }
    }
}

fn fuzz_eval(case: EvalCase, now_ms: u64) {
    let payload: Vec<Vec<u8>> = case
        .payload
        .into_iter()
        .take(6)
        .map(normalize_blob)
        .collect();
    let mut argv = vec![
        b"EVAL".to_vec(),
        b"return {#KEYS,#ARGV}".to_vec(),
        render_numkeys_arg(&case.numkeys),
    ];
    argv.extend(payload.clone());

    match modeled_eval_counts(&case.numkeys, payload.len()) {
        Some((keys, args)) => {
            let mut store = seeded_store(now_ms, false);
            let result = dispatch_argv(&argv, &mut store, now_ms);
            assert_eq!(
                result,
                Ok(RespFrame::Array(Some(vec![
                    RespFrame::Integer(keys as i64),
                    RespFrame::Integer(args as i64),
                ])))
            );
        }
        None => {
            // NumkeysArg::Invalid(blob) may render a valid integer (e.g. "1"),
            // making the command accepted; only Integer(negative) and
            // keys>args are structurally rejectable.
            let blob_ambiguous = matches!(case.numkeys, NumkeysArg::Invalid(_));
            assert_modeled_invalid(argv, blob_ambiguous, now_ms);
        }
    }
}

fn fuzz_blocking_timeout(case: BlockingTimeoutCase, now_ms: u64) {
    let timeout = render_timeout_arg(&case.timeout);
    let argv = vec![
        b"BRPOPLPUSH".to_vec(),
        b"source".to_vec(),
        b"dest".to_vec(),
        timeout.clone(),
    ];
    match canonical_timeout_arg(&case.timeout) {
        Some(canonical_timeout) => {
            let canonical = vec![
                b"BRPOPLPUSH".to_vec(),
                b"source".to_vec(),
                b"dest".to_vec(),
                canonical_timeout,
            ];
            assert_equivalent(argv, canonical, now_ms, case.has_source_value);
        }
        None => {
            // TimeoutArg::Invalid(blob) may render a valid timeout (e.g. "5" or
            // "1.5"); Negative/Nan/Infinity are structurally rejectable.
            let blob_ambiguous = matches!(case.timeout, TimeoutArg::Invalid(_));
            if blob_ambiguous {
                assert_inert_on_reject(argv, now_ms);
            } else {
                let mut store = seeded_store(now_ms, case.has_source_value);
                let before = store.state_digest();
                let result = dispatch_argv(&argv, &mut store, now_ms);
                assert!(is_rejection(&result));
                assert_eq!(before, store.state_digest());
            }
        }
    }
}

fn assert_equivalent(
    argv: Vec<Vec<u8>>,
    canonical: Vec<Vec<u8>>,
    now_ms: u64,
    has_source_value: bool,
) {
    let mut lhs = seeded_store(now_ms, has_source_value);
    let mut rhs = seeded_store(now_ms, has_source_value);
    let lhs_result = dispatch_argv(&argv, &mut lhs, now_ms);
    let rhs_result = dispatch_argv(&canonical, &mut rhs, now_ms);
    assert_eq!(lhs_result, rhs_result);
    assert_eq!(lhs.state_digest(), rhs.state_digest());
}

fn assert_rejected(argv: Vec<Vec<u8>>, now_ms: u64) {
    let mut store = seeded_store(now_ms, false);
    let before = store.state_digest();
    let result = dispatch_argv(&argv, &mut store, now_ms);
    assert!(is_rejection(&result));
    assert_eq!(before, store.state_digest());
}

/// For inputs whose validity cannot be determined a priori — an arbitrary
/// `Invalid(Blob)` may coincidentally render a perfectly valid id such as `"5"`
/// (a bare-ms id) — we cannot assert acceptance OR rejection. Assert only the
/// mutation-proof property that holds either way: a command fr REJECTS must
/// leave the keyspace untouched (argument validation precedes execution).
fn assert_inert_on_reject(argv: Vec<Vec<u8>>, now_ms: u64) {
    let mut store = seeded_store(now_ms, false);
    let before = store.state_digest();
    let result = dispatch_argv(&argv, &mut store, now_ms);
    if is_rejection(&result) {
        assert_eq!(before, store.state_digest());
    }
}

/// Route a "modeled-as-invalid" command. When the input carried an
/// arbitrary-blob token (`Invalid`/`InvalidKeyword`) the rendered bytes may
/// coincidentally be a VALID keyword/value (e.g. a blob rendering "WITHSCORES",
/// "-", or "5"), so we cannot assert rejection — only the mutation-proof
/// inert-on-reject invariant. When the invalidity is structural (a dangling
/// `Missing*` keyword, a non-positive COUNT, a negative numkeys, ...) the
/// rejection IS guaranteed, so assert it hard to keep catching real
/// over-acceptance regressions.
fn assert_modeled_invalid(argv: Vec<Vec<u8>>, blob_ambiguous: bool, now_ms: u64) {
    if blob_ambiguous {
        assert_inert_on_reject(argv, now_ms);
    } else {
        assert_rejected(argv, now_ms);
    }
}

fn assert_accepted(argv: Vec<Vec<u8>>, now_ms: u64) {
    let mut store = seeded_store(now_ms, false);
    let result = dispatch_argv(&argv, &mut store, now_ms);
    assert!(!is_rejection(&result));
}

fn is_rejection(result: &Result<RespFrame, CommandError>) -> bool {
    matches!(result, Err(_) | Ok(RespFrame::Error(_)))
}

fn seeded_store(now_ms: u64, has_source_value: bool) -> Store {
    let mut store = Store::new();
    store.set(b"scan:str".to_vec(), b"v".to_vec(), None, now_ms);
    let _ = store.hset(b"h", b"field1".to_vec(), b"value1".to_vec(), now_ms);
    let _ = store.hset(b"h", b"field2".to_vec(), b"value2".to_vec(), now_ms);
    let _ = store.lpush(b"scan:list", &[b"item".to_vec()], now_ms);
    let _ = store.zadd(
        b"zset",
        &[
            (1.0, b"alpha".to_vec()),
            (2.0, b"beta".to_vec()),
            (3.0, b"gamma".to_vec()),
            (4.0, b"delta".to_vec()),
        ],
        now_ms,
    );
    let _ = store.zadd(
        b"zs1",
        &[(1.0, b"a".to_vec()), (3.0, b"c".to_vec())],
        now_ms,
    );
    let _ = store.zadd(
        b"zs2",
        &[(2.0, b"b".to_vec()), (4.0, b"c".to_vec())],
        now_ms,
    );
    let _ = store.xadd(b"s", (1, 0), &[(b"f".to_vec(), b"1".to_vec())], now_ms);
    let _ = store.xadd(b"s", (2, 0), &[(b"f".to_vec(), b"2".to_vec())], now_ms);
    let _ = store.xgroup_create(b"s", b"g", (0, 0), false, now_ms);
    let geo_seed = vec![
        b"GEOADD".to_vec(),
        b"geo".to_vec(),
        b"13.361389".to_vec(),
        b"38.115556".to_vec(),
        b"palermo".to_vec(),
        b"15.087269".to_vec(),
        b"37.502669".to_vec(),
        b"catania".to_vec(),
    ];
    let _ = dispatch_argv(&geo_seed, &mut store, now_ms);
    if has_source_value {
        let _ = store.lpush(b"source", &[b"payload".to_vec()], now_ms);
    }
    store
}

fn model_zrange_tokens(
    tokens: &[ZRangeToken],
    argv: &mut Vec<Vec<u8>>,
) -> Option<ZRangeByScoreModel> {
    let mut model = ZRangeByScoreModel {
        withscores: false,
        limit: None,
    };
    for token in tokens.iter().take(8) {
        match token {
            ZRangeToken::WithScores => {
                argv.push(b"WITHSCORES".to_vec());
                model.withscores = true;
            }
            ZRangeToken::Limit { offset, count } => {
                argv.push(b"LIMIT".to_vec());
                argv.push(offset.to_string().into_bytes());
                argv.push(count.to_string().into_bytes());
                // Negative offset/count are BOTH accepted by upstream (plain
                // long parse). A negative offset skips beyond the end and yields
                // an empty reply; it is not a rejection. Keep it in the model so
                // the option-ordering equivalence check still runs.
                model.limit = Some((i64::from(*offset), *count));
            }
            ZRangeToken::InvalidKeyword(blob) => {
                argv.push(normalize_blob(blob.clone()));
                return None;
            }
            ZRangeToken::MissingLimitCount { offset } => {
                argv.push(b"LIMIT".to_vec());
                argv.push(offset.to_string().into_bytes());
                return None;
            }
        }
    }
    Some(model)
}

fn model_geo_flags(tokens: &[GeoFlagToken], argv: &mut Vec<Vec<u8>>) -> Option<GeoSearchModel> {
    let mut model = GeoSearchModel {
        withcoord: false,
        withdist: false,
        withhash: false,
        count: None,
        any: false,
        asc: true,
    };
    for token in tokens.iter().take(8) {
        match token {
            GeoFlagToken::WithCoord => {
                argv.push(b"WITHCOORD".to_vec());
                model.withcoord = true;
            }
            GeoFlagToken::WithDist => {
                argv.push(b"WITHDIST".to_vec());
                model.withdist = true;
            }
            GeoFlagToken::WithHash => {
                argv.push(b"WITHHASH".to_vec());
                model.withhash = true;
            }
            GeoFlagToken::Count(count) => {
                argv.push(b"COUNT".to_vec());
                argv.push(count.to_string().into_bytes());
                if *count <= 0 {
                    return None;
                }
                model.count = Some(*count);
            }
            GeoFlagToken::Any => {
                argv.push(b"ANY".to_vec());
                model.any = true;
            }
            GeoFlagToken::Asc => {
                argv.push(b"ASC".to_vec());
                model.asc = true;
            }
            GeoFlagToken::Desc => {
                argv.push(b"DESC".to_vec());
                model.asc = false;
            }
            GeoFlagToken::InvalidKeyword(blob) => {
                argv.push(normalize_blob(blob.clone()));
                return None;
            }
            GeoFlagToken::MissingCountValue => {
                argv.push(b"COUNT".to_vec());
                return None;
            }
        }
    }
    Some(model)
}

fn model_zstore_tokens(tokens: &[ZStoreToken], argv: &mut Vec<Vec<u8>>) -> Option<ZStoreModel> {
    let mut model = ZStoreModel {
        weights: None,
        aggregate: None,
    };
    for token in tokens.iter().take(6) {
        match token {
            ZStoreToken::Weights { left, right } => {
                argv.push(b"WEIGHTS".to_vec());
                argv.push(left.to_string().into_bytes());
                argv.push(right.to_string().into_bytes());
                model.weights = Some((*left, *right));
            }
            ZStoreToken::Aggregate(aggregate) => {
                argv.push(b"AGGREGATE".to_vec());
                argv.push(aggregate.as_bytes().to_vec());
                model.aggregate = Some(*aggregate);
            }
            ZStoreToken::InvalidKeyword(blob) => {
                argv.push(normalize_blob(blob.clone()));
                return None;
            }
            ZStoreToken::MissingWeight => {
                argv.push(b"WEIGHTS".to_vec());
                argv.push(b"1".to_vec());
                return None;
            }
            ZStoreToken::MissingAggregateValue => {
                argv.push(b"AGGREGATE".to_vec());
                return None;
            }
        }
    }
    Some(model)
}

fn model_scan_tokens(tokens: &[ScanToken], argv: &mut Vec<Vec<u8>>) -> Option<ScanModel> {
    let mut model = ScanModel {
        pattern: None,
        count: None,
        type_name: None,
        novalues: false,
    };
    for token in tokens.iter().take(8) {
        match token {
            ScanToken::Match(blob) => {
                argv.push(b"MATCH".to_vec());
                let pattern = nonempty_blob(blob.clone(), b"*");
                argv.push(pattern.clone());
                model.pattern = Some(pattern);
            }
            ScanToken::Count(count) => {
                argv.push(b"COUNT".to_vec());
                argv.push(count.to_string().into_bytes());
                if *count <= 0 {
                    return None;
                }
                model.count = Some(*count);
            }
            ScanToken::Type(type_name) => {
                argv.push(b"TYPE".to_vec());
                argv.push(type_name.as_bytes().to_vec());
                model.type_name = Some(*type_name);
            }
            ScanToken::NoValues => {
                argv.push(b"NOVALUES".to_vec());
                model.novalues = true;
            }
            ScanToken::InvalidKeyword(blob) => {
                argv.push(normalize_blob(blob.clone()));
                return None;
            }
            ScanToken::MissingMatchValue => {
                argv.push(b"MATCH".to_vec());
                return None;
            }
            ScanToken::MissingCountValue => {
                argv.push(b"COUNT".to_vec());
                return None;
            }
            ScanToken::MissingTypeValue => {
                argv.push(b"TYPE".to_vec());
                return None;
            }
        }
    }
    Some(model)
}

fn render_stream_id_arg(id: &StreamIdArg) -> Vec<u8> {
    match id {
        StreamIdArg::Explicit { ms, seq } => format!("{ms}-{seq}").into_bytes(),
        StreamIdArg::BareMs(ms) => ms.to_string().into_bytes(),
        StreamIdArg::Dollar => b"$".to_vec(),
        StreamIdArg::Dash => b"-".to_vec(),
        StreamIdArg::Plus => b"+".to_vec(),
        StreamIdArg::Invalid(blob) => normalize_blob(blob.clone()),
    }
}

fn render_stream_bound_arg(bound: &StreamBoundArg) -> Vec<u8> {
    match bound {
        StreamBoundArg::Explicit { ms, seq } => format!("{ms}-{seq}").into_bytes(),
        StreamBoundArg::BareMs(ms) => ms.to_string().into_bytes(),
        StreamBoundArg::Dash => b"-".to_vec(),
        StreamBoundArg::Plus => b"+".to_vec(),
        StreamBoundArg::Invalid(blob) => normalize_blob(blob.clone()),
    }
}

fn canonical_xread_id(id: &StreamIdArg) -> Option<Vec<u8>> {
    match id {
        StreamIdArg::Explicit { ms, seq } => Some(format!("{ms}-{seq}").into_bytes()),
        StreamIdArg::BareMs(ms) => Some(format!("{ms}-0").into_bytes()),
        StreamIdArg::Dollar | StreamIdArg::Dash | StreamIdArg::Plus | StreamIdArg::Invalid(_) => {
            None
        }
    }
}

fn canonical_stream_range_bound(bound: &StreamBoundArg, is_start: bool) -> Option<Vec<u8>> {
    match bound {
        StreamBoundArg::Explicit { ms, seq } => Some(format!("{ms}-{seq}").into_bytes()),
        StreamBoundArg::BareMs(ms) => {
            Some(format!("{ms}-{}", if is_start { 0 } else { u64::MAX }).into_bytes())
        }
        StreamBoundArg::Dash => Some(b"-".to_vec()),
        StreamBoundArg::Plus => Some(b"+".to_vec()),
        StreamBoundArg::Invalid(_) => None,
    }
}

fn render_numkeys_arg(arg: &NumkeysArg) -> Vec<u8> {
    match arg {
        NumkeysArg::Integer(value) => value.to_string().into_bytes(),
        NumkeysArg::Invalid(blob) => normalize_blob(blob.clone()),
    }
}

fn modeled_eval_counts(numkeys: &NumkeysArg, total_after: usize) -> Option<(usize, usize)> {
    match numkeys {
        NumkeysArg::Integer(value) if *value >= 0 => {
            let keys = *value as usize;
            if keys > total_after {
                None
            } else {
                Some((keys, total_after - keys))
            }
        }
        _ => None,
    }
}

fn render_timeout_arg(arg: &TimeoutArg) -> Vec<u8> {
    match arg {
        TimeoutArg::Integer(value) => value.to_string().into_bytes(),
        TimeoutArg::Decimal { whole, frac } => format!("{whole}.{frac}").into_bytes(),
        TimeoutArg::Scientific(value) => format!("{value}e0").into_bytes(),
        TimeoutArg::Negative(value) => format!("-{value}").into_bytes(),
        TimeoutArg::Infinity => b"inf".to_vec(),
        TimeoutArg::Nan => b"NaN".to_vec(),
        TimeoutArg::Invalid(blob) => normalize_blob(blob.clone()),
    }
}

fn canonical_timeout_arg(arg: &TimeoutArg) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(&render_timeout_arg(arg))
        .ok()?
        .to_string();
    let timeout: f64 = text.parse().ok()?;
    if !timeout.is_finite() || timeout < 0.0 {
        return None;
    }
    Some(timeout.to_string().into_bytes())
}

fn normalize_blob(blob: Blob) -> Vec<u8> {
    truncate_bytes(blob.0, MAX_BLOB_LEN)
}

fn nonempty_blob(blob: Blob, fallback: &[u8]) -> Vec<u8> {
    let bytes = normalize_blob(blob);
    if bytes.is_empty() {
        fallback.to_vec()
    } else {
        bytes
    }
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}

impl AggregateKind {
    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Sum => b"SUM",
            Self::Min => b"MIN",
            Self::Max => b"MAX",
        }
    }
}

impl TypeName {
    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::String => b"string",
            Self::Hash => b"hash",
            Self::List => b"list",
            Self::Set => b"set",
            Self::Zset => b"zset",
            Self::Stream => b"stream",
            Self::Bogus => b"bogus",
        }
    }
}
