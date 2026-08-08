//! (frankenredis-a0wt5) Snapshot of what a script PROPAGATES to replicas/AOF.
//!
//! `redis.call`'s `argv` buffer is recycled across calls. That buffer outlives
//! dispatch — it feeds `record_script_monitor`, `command_may_propagate_from_script`
//! and the effect-rewrite path — so the risk of recycling it is not the
//! allocation, it is silently changing what replicas and the AOF receive.
//!
//! This pins the propagated effect stream for a corpus that deliberately
//! includes the non-deterministic commands `frankenredis-x1225` rewrites
//! (SPOP, INCRBYFLOAT, XADD `*`), a read-only call that must propagate NOTHING,
//! PUBLISH (which propagates without dirtying), and multi-call scripts where a
//! recycled buffer would be most likely to bleed one call into the next.
//!
//! It is a differential gate, not a unit test: the expected values were
//! captured from the pre-recycling implementation and must survive unchanged.

use fr_command::dispatch_argv;
use fr_command::lua_eval::eval_script;
use fr_store::Store;

fn run(script: &[u8], keys: &[Vec<u8>], argv: &[Vec<u8>]) -> Vec<String> {
    let mut store = Store::new();
    // Seed deterministic state for the commands that need it.
    for setup in [
        vec![b"SET".to_vec(), b"str".to_vec(), b"1".to_vec()],
        vec![b"SET".to_vec(), b"f".to_vec(), b"1.5".to_vec()],
        vec![b"SADD".to_vec(), b"set".to_vec(), b"only".to_vec()],
        vec![b"RPUSH".to_vec(), b"list".to_vec(), b"a".to_vec()],
    ] {
        let _ = dispatch_argv(&setup, &mut store, 0);
    }
    store.script_propagation_records.clear();
    let _ = eval_script(script, keys, argv, &mut store, 1_700_000_000_000);
    store
        .script_propagation_records
        .iter()
        .map(|record| {
            let parts: Vec<String> = record
                .argv
                .iter()
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect();
            parts.join(" ")
        })
        .collect()
}

fn snapshot() -> Vec<(&'static str, Vec<String>)> {
    let cases: Vec<(&'static str, &[u8])> = vec![
        ("read_only_get", b"redis.call('GET','str') return 1"),
        ("single_set", b"redis.call('SET','k','v') return 1"),
        ("incr", b"redis.call('INCR','str') return 1"),
        (
            "incrbyfloat_rewrite",
            b"redis.call('INCRBYFLOAT','f','0.25') return 1",
        ),
        ("spop_rewrite", b"redis.call('SPOP','set') return 1"),
        (
            "xadd_star_rewrite",
            b"redis.call('XADD','stream','*','field','value') return 1",
        ),
        (
            "publish_without_dirty",
            b"redis.call('PUBLISH','c','m') return 1",
        ),
        (
            "many_calls_same_buffer",
            b"for i=1,5 do redis.call('SET','k'..i,'v'..i) end return 1",
        ),
        (
            "mixed_read_and_write_interleaved",
            b"for i=1,4 do redis.call('GET','str') redis.call('SET','m'..i,'x') end return 1",
        ),
        (
            "varying_arity_across_calls",
            b"redis.call('SET','a','1') redis.call('SETEX','b',100,'2') \
              redis.call('DEL','a') return 1",
        ),
        (
            "pcall_error_then_write",
            b"redis.pcall('INCR','set') redis.call('SET','after','1') return 1",
        ),
        (
            "write_then_failing_call",
            b"redis.call('SET','before','1') local ok = pcall(function() \
              redis.call('WRONGCMD') end) return 1",
        ),
        (
            "keys_argv_write",
            b"redis.call('SET',KEYS[1],ARGV[1]) return 1",
        ),
    ];
    cases
        .into_iter()
        .map(|(name, script)| {
            let out = run(script, &[b"kkey".to_vec()], &[b"vval".to_vec()]);
            (name, out)
        })
        .collect()
}

#[test]
fn script_propagation_effects_are_stable() {
    // Captured from the implementation BEFORE argv recycling (commit 5e1d28527)
    // and required to match afterwards.
    let expected: Vec<(&str, Vec<&str>)> = vec![
        ("read_only_get", vec![]),
        ("single_set", vec!["SET k v"]),
        ("incr", vec!["INCR str"]),
        // INCRBYFLOAT propagates the resolved value AND preserves the TTL.
        ("incrbyfloat_rewrite", vec!["SET f 1.75 KEEPTTL"]),
        // SPOP on a one-member set empties it, so the effect is DEL, not SREM.
        ("spop_rewrite", vec!["DEL set"]),
        // The `*` id is resolved to a concrete one before propagation.
        (
            "xadd_star_rewrite",
            vec!["XADD stream 1700000000000-0 field value"],
        ),
        ("publish_without_dirty", vec!["PUBLISH c m"]),
        (
            "many_calls_same_buffer",
            vec![
                "SET k1 v1",
                "SET k2 v2",
                "SET k3 v3",
                "SET k4 v4",
                "SET k5 v5",
            ],
        ),
        (
            "mixed_read_and_write_interleaved",
            vec!["SET m1 x", "SET m2 x", "SET m3 x", "SET m4 x"],
        ),
        // SETEX propagates as SET with an ABSOLUTE deadline, so a replica
        // replaying later cannot drift the expiry.
        (
            "varying_arity_across_calls",
            vec!["SET a 1", "SET b 2 PXAT 1700000100000", "DEL a"],
        ),
        ("pcall_error_then_write", vec!["SET after 1"]),
        ("write_then_failing_call", vec!["SET before 1"]),
        ("keys_argv_write", vec!["SET kkey vval"]),
    ];

    let actual = snapshot();
    // Printed so a failure shows the whole stream, and so this file can be
    // captured against another revision to diff the two directly.
    for (name, effects) in &actual {
        println!("PROPAGATION {name} = {effects:?}");
    }
    assert_eq!(
        actual.len(),
        expected.len(),
        "case count changed; update the snapshot deliberately"
    );
    for ((name, got), (expected_name, want)) in actual.iter().zip(expected.iter()) {
        assert_eq!(name, expected_name, "case order changed");
        let want: Vec<String> = want.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(
            got, &want,
            "propagated effects changed for {name}: a recycled argv buffer must not alter \
what replicas and the AOF receive"
        );
    }
}
