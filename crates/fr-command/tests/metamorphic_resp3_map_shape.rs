//! Cross-cutting metamorphic invariants for the RESP3-Map vs RESP2-
//! Array shape contract on introspection commands. Pins the family
//! that recently flipped to dual-shape — i40x2 (CLIENT TRACKINGINFO),
//! e4njz (ACL GETUSER), udl3y (MEMORY STATS), asvh1 (FUNCTION STATS).
//!
//! Each invariant compares the RESP2 and RESP3 outputs of the SAME
//! command + state and asserts the shape contract:
//!   - RESP2 Array length is 2× the RESP3 Map entry count.
//!   - The keys in the RESP3 Map appear at even indices in the
//!     RESP2 Array, in the same order.
//!   - The values in the RESP3 Map appear at odd indices, in the
//!     same order.
//!
//! A future refactor that touches the resp_protocol_version branch
//! in one handler but breaks another now gets caught at the family
//! level instead of slipping past the per-command pin tests.
//!
//! (frankenredis-57j9w)
//!
//! ACL GETUSER lives behind the runtime layer (handle_acl_getuser
//! reads self.session.resp_protocol_version), so its invariant is
//! exercised in fr-runtime's own test suite. This file covers the
//! three fr-command-resident handlers that route through
//! dispatch_argv with store.dispatch_client_ctx.resp_protocol_version.

use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

/// Run `argv` through dispatch_argv twice — once with RESP2, once
/// with RESP3 — using a fresh store (so deterministic state). Returns
/// (resp2_reply, resp3_reply).
fn run_both_protocols(argv: &[Vec<u8>]) -> (RespFrame, RespFrame) {
    let mut store = Store::new();
    // Default protocol_version = 2 (Store::new()).
    let resp2 = dispatch_argv(argv, &mut store, 0).expect("RESP2 dispatch");

    let mut store = Store::new();
    store.dispatch_client_ctx.resp_protocol_version = 3;
    let resp3 = dispatch_argv(argv, &mut store, 0).expect("RESP3 dispatch");

    (resp2, resp3)
}

/// Walk a flat Array of alternating k/v entries and pull out the
/// (key, value) chunks. Panics if the array length is odd or any
/// chunk has the wrong shape.
fn flat_array_to_pairs(items: &[RespFrame]) -> Vec<(RespFrame, RespFrame)> {
    assert!(
        items.len().is_multiple_of(2),
        "flat array must have even length for k/v shape, got {}",
        items.len()
    );
    items
        .chunks(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect()
}

/// (frankenredis-ndz0j) Some commands (e.g. MEMORY STATS) carry nested
/// Map↔flat-Array pairs at the value level too. Recursively rewrite a
/// RESP3 Map into the equivalent RESP2 flat-Array shape so the two
/// trees can be compared structurally without false negatives at
/// nested layers.
fn canonicalize_to_resp2_shape(frame: &RespFrame) -> RespFrame {
    match frame {
        RespFrame::Map(Some(entries)) => {
            let mut flat = Vec::with_capacity(entries.len() * 2);
            for (k, v) in entries {
                flat.push(canonicalize_to_resp2_shape(k));
                flat.push(canonicalize_to_resp2_shape(v));
            }
            RespFrame::Array(Some(flat))
        }
        RespFrame::Array(Some(items)) => RespFrame::Array(Some(
            items.iter().map(canonicalize_to_resp2_shape).collect(),
        )),
        // RESP3 Double -> RESP2 BulkString of the same ASCII representation
        RespFrame::Double(s) => RespFrame::BulkString(Some(s.as_bytes().to_vec())),
        // RESP3 Set -> RESP2 Array
        RespFrame::Set(Some(items)) => RespFrame::Array(Some(
            items.iter().map(canonicalize_to_resp2_shape).collect(),
        )),
        RespFrame::Set(None) => RespFrame::Array(None),
        other => other.clone(),
    }
}

/// Assert RESP2 flat Array of length 2N matches RESP3 Map of length N
/// in key order + paired values. Nested Map↔flat-Array divergences at
/// the value level are normalized via canonicalize_to_resp2_shape.
fn assert_shape_contract(label: &str, resp2: &RespFrame, resp3: &RespFrame) {
    let RespFrame::Array(Some(items)) = resp2 else {
        panic!("{label}: RESP2 reply must be Array, got {resp2:?}"); // ubs:ignore — AI triage
    };
    let RespFrame::Map(Some(entries)) = resp3 else {
        panic!("{label}: RESP3 reply must be Map, got {resp3:?}"); // ubs:ignore — AI triage
    };
    assert_eq!(
        items.len(),
        entries.len() * 2,
        "{label}: RESP2 Array length must be 2× RESP3 Map entry count"
    );
    let array_pairs = flat_array_to_pairs(items);
    for (i, ((map_k, map_v), (arr_k, arr_v))) in entries.iter().zip(array_pairs.iter()).enumerate()
    {
        let map_k_norm = canonicalize_to_resp2_shape(map_k);
        let arr_k_norm = canonicalize_to_resp2_shape(arr_k);
        assert_eq!(
            map_k_norm, arr_k_norm,
            "{label}: pair {i} key mismatch — Map key {map_k:?} != Array key {arr_k:?}"
        );
        let map_v_norm = canonicalize_to_resp2_shape(map_v);
        let arr_v_norm = canonicalize_to_resp2_shape(arr_v);
        assert_eq!(
            map_v_norm, arr_v_norm,
            "{label}: pair {i} value mismatch — Map value {map_v:?} != Array value {arr_v:?}"
        );
    }
}

#[test]
fn mr_client_trackinginfo_resp2_array_matches_resp3_map_shape() {
    // i40x2 contract: 3-entry Map ↔ 6-element Array.
    let (resp2, resp3) = run_both_protocols(&[b"CLIENT".to_vec(), b"TRACKINGINFO".to_vec()]);
    assert_shape_contract("CLIENT TRACKINGINFO", &resp2, &resp3);

    // Spot-check the 3 expected keys.
    let RespFrame::Map(Some(entries)) = &resp3 else {
        unreachable!("guarded above"); // ubs:ignore — AI triage
    };
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].0, RespFrame::BulkString(Some(b"flags".to_vec())));
    assert_eq!(
        entries[1].0,
        RespFrame::BulkString(Some(b"redirect".to_vec()))
    );
    assert_eq!(
        entries[2].0,
        RespFrame::BulkString(Some(b"prefixes".to_vec()))
    );
}

#[test]
fn mr_memory_stats_resp2_array_matches_resp3_map_shape() {
    // udl3y contract: ~30+ entries in both shapes.
    let (resp2, resp3) = run_both_protocols(&[b"MEMORY".to_vec(), b"STATS".to_vec()]);
    assert_shape_contract("MEMORY STATS", &resp2, &resp3);

    let RespFrame::Map(Some(entries)) = &resp3 else {
        unreachable!("guarded above"); // ubs:ignore — AI triage
    };
    assert!(
        entries.len() >= 20,
        "MEMORY STATS Map should have >=20 entries, got {}",
        entries.len()
    );
    // First 3 keys are the well-known openers.
    assert_eq!(
        entries[0].0,
        RespFrame::BulkString(Some(b"peak.allocated".to_vec()))
    );
    assert_eq!(
        entries[1].0,
        RespFrame::BulkString(Some(b"total.allocated".to_vec()))
    );
    assert_eq!(
        entries[2].0,
        RespFrame::BulkString(Some(b"startup.allocated".to_vec()))
    );
}

#[test]
fn mr_function_stats_resp3_outer_keys_match_resp2_array_keys() {
    // asvh1 contract is doubly-nested: outer 2-entry Map, inner
    // engines Map, per-engine Map. The flat-Array form has the same
    // outer keys at even indices; the nested values diverge in shape
    // (Array<...> in RESP2 vs Map<...> in RESP3). Pin only the
    // outer-key invariant — the nested shape is verified by asvh1's
    // per-command tests.
    let (resp2, resp3) = run_both_protocols(&[b"FUNCTION".to_vec(), b"STATS".to_vec()]);

    let RespFrame::Array(Some(items)) = &resp2 else {
        panic!("RESP2 FUNCTION STATS must be flat Array, got {resp2:?}"); // ubs:ignore — AI triage
    };
    let RespFrame::Map(Some(entries)) = &resp3 else {
        panic!("RESP3 FUNCTION STATS must be Map, got {resp3:?}"); // ubs:ignore — AI triage
    };
    assert_eq!(items.len(), 4, "RESP2 outer is 4-element flat array");
    assert_eq!(entries.len(), 2, "RESP3 outer is 2-entry Map");
    // Keys match at even indices.
    assert_eq!(items[0], entries[0].0);
    assert_eq!(items[2], entries[1].0);
    // Outer key names are stable.
    assert_eq!(
        entries[0].0,
        RespFrame::BulkString(Some(b"running_script".to_vec()))
    );
    assert_eq!(
        entries[1].0,
        RespFrame::BulkString(Some(b"engines".to_vec()))
    );
}

/// Run argv against a pre-seeded store under RESP2 and RESP3.
/// The `seed` closure is called twice with a fresh store each time so
/// the two protocol runs see byte-identical state.
fn run_both_protocols_with_seed(
    seed: impl Fn(&mut Store) + Copy,
    argv: &[Vec<u8>],
) -> (RespFrame, RespFrame) {
    let mut store = Store::new();
    seed(&mut store);
    let resp2 = dispatch_argv(argv, &mut store, 0).expect("RESP2 dispatch");

    let mut store = Store::new();
    seed(&mut store);
    store.dispatch_client_ctx.resp_protocol_version = 3;
    let resp3 = dispatch_argv(argv, &mut store, 0).expect("RESP3 dispatch");

    (resp2, resp3)
}

#[test]
fn mr_zrange_withscores_resp3_pairs_match_resp2_flat_alternating() {
    // jnf53 contract: under RESP2+WITHSCORES the wire is flat
    // [m1, s1, m2, s2, ...]; under RESP3+WITHSCORES it's
    // [[m1, s1], [m2, s2], ...]. The (member, score) tuples extracted
    // from each form must be byte-identical and ordered.
    //
    // Family covered by a shared helper (zrange_emit_with_resp): a
    // future regression that breaks one ingestion path also breaks the
    // others. Pin three of them — ZRANGE rank, ZRANGEBYSCORE,
    // ZRANGE BYSCORE REV — to catch helper rewires.
    let seed = |store: &mut Store| {
        dispatch_argv(
            &[
                b"ZADD".to_vec(),
                b"zs".to_vec(),
                b"1".to_vec(),
                b"a".to_vec(),
                b"2".to_vec(),
                b"b".to_vec(),
                b"3".to_vec(),
                b"c".to_vec(),
            ],
            store,
            0,
        )
        .expect("zadd");
    };

    for argv in [
        vec![
            b"ZRANGE".to_vec(),
            b"zs".to_vec(),
            b"0".to_vec(),
            b"-1".to_vec(),
            b"WITHSCORES".to_vec(),
        ],
        vec![
            b"ZRANGEBYSCORE".to_vec(),
            b"zs".to_vec(),
            b"-inf".to_vec(),
            b"+inf".to_vec(),
            b"WITHSCORES".to_vec(),
        ],
        vec![
            b"ZRANGE".to_vec(),
            b"zs".to_vec(),
            b"+inf".to_vec(),
            b"-inf".to_vec(),
            b"BYSCORE".to_vec(),
            b"REV".to_vec(),
            b"WITHSCORES".to_vec(),
        ],
    ] {
        let (resp2, resp3) = run_both_protocols_with_seed(seed, &argv);
        let RespFrame::Array(Some(flat)) = &resp2 else {
            panic!("{argv:?}: RESP2 must be flat Array, got {resp2:?}"); // ubs:ignore — AI triage
        };
        let RespFrame::Array(Some(pairs)) = &resp3 else {
            panic!("{argv:?}: RESP3 must be outer Array, got {resp3:?}"); // ubs:ignore — AI triage
        };
        assert!(
            flat.len().is_multiple_of(2),
            "{argv:?}: RESP2 flat array must have even length"
        );
        assert_eq!(
            flat.len(),
            pairs.len() * 2,
            "{argv:?}: RESP2 flat length must equal 2× RESP3 outer pair count"
        );
        for (i, pair) in pairs.iter().enumerate() {
            let RespFrame::Array(Some(pair_items)) = pair else {
                panic!("{argv:?}: RESP3 element {i} must be 2-Array, got {pair:?}"); // ubs:ignore — AI triage
            };
            assert_eq!(pair_items.len(), 2);
            assert_eq!(
                pair_items[0],
                flat[i * 2],
                "{argv:?}: pair {i} member byte drift"
            );
            // RESP3 scores are Double, RESP2 are BulkString — canonicalize before comparing
            assert_eq!(
                canonicalize_to_resp2_shape(&pair_items[1]),
                flat[i * 2 + 1],
                "{argv:?}: pair {i} score byte drift"
            );
        }
    }
}

#[test]
fn mr_zpop_with_count_resp3_pairs_match_resp2_flat_alternating() {
    // 1g3ao contract: ZPOPMIN/ZPOPMAX with COUNT under RESP3 wraps
    // each (member, score) in a 2-Array; under RESP2 stays flat. Since
    // ZPOP destructively pops, seed-twice and pop both for ZPOPMIN
    // and (separately) for ZPOPMAX with the same count.
    for command in [&b"ZPOPMIN"[..], &b"ZPOPMAX"[..]] {
        let seed = |store: &mut Store| {
            dispatch_argv(
                &[
                    b"ZADD".to_vec(),
                    b"zs".to_vec(),
                    b"1".to_vec(),
                    b"a".to_vec(),
                    b"2".to_vec(),
                    b"b".to_vec(),
                    b"3".to_vec(),
                    b"c".to_vec(),
                ],
                store,
                0,
            )
            .expect("zadd");
        };
        let argv = vec![command.to_vec(), b"zs".to_vec(), b"2".to_vec()];
        let (resp2, resp3) = run_both_protocols_with_seed(seed, &argv);
        let RespFrame::Array(Some(flat)) = &resp2 else {
            panic!("RESP2 {command:?} must be flat Array, got {resp2:?}"); // ubs:ignore — AI triage
        };
        let RespFrame::Array(Some(pairs)) = &resp3 else {
            panic!("RESP3 {command:?} must be outer Array, got {resp3:?}"); // ubs:ignore — AI triage
        };
        assert_eq!(
            flat.len(),
            pairs.len() * 2,
            "{command:?}: RESP2 flat length == 2× RESP3 pair count"
        );
        for (i, pair) in pairs.iter().enumerate() {
            let RespFrame::Array(Some(pair_items)) = pair else {
                panic!("{command:?}: RESP3 element {i} must be 2-Array"); // ubs:ignore — AI triage
            };
            assert_eq!(pair_items[0], flat[i * 2]);
            // RESP3 scores are Double, RESP2 are BulkString — canonicalize before comparing
            assert_eq!(canonicalize_to_resp2_shape(&pair_items[1]), flat[i * 2 + 1]);
        }
    }
}

#[test]
fn mr_lcs_idx_resp3_map_outer_matches_resp2_flat_alternating() {
    // cz712 contract: LCS IDX under RESP2 emits flat alternating Array
    // of 4 entries (matches, len, [WITHMATCHLEN ...]); under RESP3
    // emits Map of 2 entries. The well-known keys are the same in both
    // forms.
    let seed = |store: &mut Store| {
        dispatch_argv(
            &[b"SET".to_vec(), b"k1".to_vec(), b"ohmytext".to_vec()],
            store,
            0,
        )
        .expect("set k1");
        dispatch_argv(
            &[b"SET".to_vec(), b"k2".to_vec(), b"mynewtext".to_vec()],
            store,
            0,
        )
        .expect("set k2");
    };
    let (resp2, resp3) = run_both_protocols_with_seed(
        seed,
        &[
            b"LCS".to_vec(),
            b"k1".to_vec(),
            b"k2".to_vec(),
            b"IDX".to_vec(),
        ],
    );
    assert_shape_contract("LCS IDX", &resp2, &resp3);

    let RespFrame::Map(Some(entries)) = &resp3 else {
        unreachable!("guarded by assert_shape_contract"); // ubs:ignore — AI triage
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].0,
        RespFrame::BulkString(Some(b"matches".to_vec()))
    );
    assert_eq!(entries[1].0, RespFrame::BulkString(Some(b"len".to_vec())));
}

#[test]
fn mr_latency_histogram_resp3_map_outer_matches_resp2_flat_alternating() {
    // cgjlc contract: LATENCY HISTOGRAM emits a 3-deep nested Map under
    // RESP3 / 3-deep nested flat-Array under RESP2. Pin the outer
    // shape contract: cmd_name keys at even RESP2 indices = RESP3
    // outer Map keys.
    let seed = |store: &mut Store| {
        store.record_command_histogram("GET", 100);
        store.record_command_histogram("GET", 50);
        store.record_command_histogram("SET", 200);
    };
    let argv = vec![b"LATENCY".to_vec(), b"HISTOGRAM".to_vec()];
    let (resp2, resp3) = run_both_protocols_with_seed(seed, &argv);

    let RespFrame::Array(Some(flat)) = &resp2 else {
        panic!("RESP2 LATENCY HISTOGRAM must be flat Array"); // ubs:ignore — AI triage
    };
    let RespFrame::Map(Some(outer)) = &resp3 else {
        panic!("RESP3 LATENCY HISTOGRAM must be Map"); // ubs:ignore — AI triage
    };
    assert_eq!(flat.len(), outer.len() * 2);
    // Keys (cmd names) at even RESP2 indices must match RESP3 outer
    // Map keys in order.
    for (i, (k, _v)) in outer.iter().enumerate() {
        assert_eq!(k, &flat[i * 2], "outer cmd-name drift at index {i}");
    }
    // Each value (CDF) under RESP3 is a Map with 'calls' +
    // 'histogram_usec' keys; under RESP2 it's a flat 4-entry Array
    // with the same keys at even indices.
    for (i, (_k, v)) in outer.iter().enumerate() {
        let RespFrame::Map(Some(cdf)) = v else {
            panic!("RESP3 CDF at {i} must be Map"); // ubs:ignore — AI triage
        };
        assert_eq!(cdf.len(), 2);
        assert_eq!(cdf[0].0, RespFrame::BulkString(Some(b"calls".to_vec())));
        assert_eq!(
            cdf[1].0,
            RespFrame::BulkString(Some(b"histogram_usec".to_vec()))
        );
        let RespFrame::Array(Some(cdf_flat)) = &flat[i * 2 + 1] else {
            panic!("RESP2 CDF at {i} must be flat Array"); // ubs:ignore — AI triage
        };
        assert_eq!(cdf_flat.len(), 4);
        assert_eq!(cdf_flat[0], cdf[0].0);
        assert_eq!(cdf_flat[2], cdf[1].0);
    }
}

#[test]
fn mr_protocol_switch_does_not_alter_data() {
    // The wire shape must change with protocol but the underlying
    // *data* must not. After running RESP2 and RESP3 in lockstep
    // against the same fresh store, the (key, value) tuples
    // extracted from each form must be byte-identical for
    // CLIENT TRACKINGINFO and MEMORY STATS. (Not applicable to
    // FUNCTION STATS — its inner shapes diverge by design.)
    for argv in [
        vec![b"CLIENT".to_vec(), b"TRACKINGINFO".to_vec()],
        vec![b"MEMORY".to_vec(), b"STATS".to_vec()],
    ] {
        let (resp2, resp3) = run_both_protocols(&argv);
        let RespFrame::Array(Some(items)) = &resp2 else {
            panic!("expected Array for {argv:?}"); // ubs:ignore — AI triage
        };
        let RespFrame::Map(Some(entries)) = &resp3 else {
            panic!("expected Map for {argv:?}"); // ubs:ignore — AI triage
        };
        let array_pairs = flat_array_to_pairs(items);
        for (i, ((map_k, map_v), (arr_k, arr_v))) in
            entries.iter().zip(array_pairs.iter()).enumerate()
        {
            // (frankenredis-ndz0j) Nested Map↔flat-Array divergences
            // are intentional under RESP3 (e.g. MEMORY STATS db.<n>
            // sub-map) — normalize before comparing.
            assert_eq!(
                canonicalize_to_resp2_shape(map_k),
                canonicalize_to_resp2_shape(arr_k),
                "{argv:?} pair {i} key drift across protocol switch"
            );
            assert_eq!(
                canonicalize_to_resp2_shape(map_v),
                canonicalize_to_resp2_shape(arr_v),
                "{argv:?} pair {i} value drift across protocol switch"
            );
        }
    }
}
