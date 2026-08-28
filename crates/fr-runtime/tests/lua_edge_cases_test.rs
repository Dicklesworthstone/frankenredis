//! Integration tests for Lua scripting edge cases: closures, recursion,
//! varargs, multi-return, error propagation, and redis.call interaction.

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
            .collect(),
    ))
}

fn eval(rt: &mut Runtime, script: &str, numkeys: &str, args: &[&[u8]]) -> RespFrame {
    let mut parts: Vec<&[u8]> = vec![b"EVAL", script.as_bytes(), numkeys.as_bytes()];
    parts.extend(args);
    rt.execute_frame(command(&parts), 0)
}

// ── Closures & upvalues ─────────────────────────────

#[test]
fn lua_closure_captures_upvalue_read_only() {
    let mut rt = Runtime::default_strict();
    // Closures can READ captured upvalues (value-copy semantics)
    let result = eval(
        &mut rt,
        r#"
        local x = 42
        local function get_x() return x end
        return get_x()
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(42));
}

#[test]
fn lua_closure_observes_updated_upvalue() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local x = 10
        local function get_x() return x end
        x = 99
        return get_x()
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(99));
}

#[test]
fn lua_sibling_closures_share_mutated_upvalue() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local x = 0
        local function inc()
            x = x + 1
        end
        local function get()
            return x
        end
        inc()
        inc()
        return get()
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(2));
}

// ── Recursion ───────────────────────────────────────

#[test]
fn lua_recursive_factorial() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local function fact(n)
            if n <= 1 then return 1 end
            return n * fact(n - 1)
        end
        return fact(10)
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(3_628_800));
}

#[test]
fn lua_recursive_fibonacci() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local function fib(n)
            if n <= 1 then return n end
            return fib(n-1) + fib(n-2)
        end
        return fib(10)
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(55));
}

// ── Multiple return values ──────────────────────────

#[test]
fn lua_multiple_return_values_table() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local function swap(a, b)
            return b, a
        end
        local x, y = swap(1, 2)
        return {x, y}
        "#,
        "0",
        &[],
    );
    assert_eq!(
        result,
        RespFrame::Array(Some(vec![RespFrame::Integer(2), RespFrame::Integer(1),]))
    );
}

#[test]
fn lua_select_with_multiple_returns() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local function multi() return 10, 20, 30 end
        local a, b, c = multi()
        return a + b + c
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::Integer(60));
}

// ── redis.call / redis.pcall interaction ────────────

#[test]
fn lua_redis_call_set_get_roundtrip() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        redis.call('SET', KEYS[1], ARGV[1])
        return redis.call('GET', KEYS[1])
        "#,
        "1",
        &[b"lua_key", b"lua_value"],
    );
    assert_eq!(result, RespFrame::BulkString(Some(b"lua_value".to_vec())));
}

#[test]
fn lua_redis_pcall_catches_wrongtype() {
    let mut rt = Runtime::default_strict();
    // Set up a string key, then try LPUSH on it via pcall
    rt.execute_frame(command(&[b"SET", b"strkey", b"val"]), 0);
    let result = eval(
        &mut rt,
        r#"
        local ok, err = pcall(redis.call, 'LPUSH', 'strkey', 'item')
        if ok then
            return 'unexpected_success'
        else
            return 'caught_error'
        end
        "#,
        "0",
        &[],
    );
    assert_eq!(
        result,
        RespFrame::BulkString(Some(b"caught_error".to_vec()))
    );
}

#[test]
fn lua_redis_call_incr_loop() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        for i = 1, 5 do
            redis.call('INCR', KEYS[1])
        end
        return redis.call('GET', KEYS[1])
        "#,
        "1",
        &[b"counter_key"],
    );
    assert_eq!(result, RespFrame::BulkString(Some(b"5".to_vec())));
}

// ── KEYS and ARGV edge cases ────────────────────────

#[test]
fn lua_empty_keys_and_argv() {
    let mut rt = Runtime::default_strict();
    let result = eval(&mut rt, r#"return #KEYS + #ARGV"#, "0", &[]);
    assert_eq!(result, RespFrame::Integer(0));
}

#[test]
fn lua_keys_out_of_bounds() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local v = KEYS[99]
        if v == nil then return 'nil_as_expected' end
        return v
        "#,
        "1",
        &[b"only_key"],
    );
    assert_eq!(
        result,
        RespFrame::BulkString(Some(b"nil_as_expected".to_vec()))
    );
}

#[test]
fn lua_argv_multiple_values() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local sum = 0
        for i = 1, #ARGV do
            sum = sum + tonumber(ARGV[i])
        end
        return sum
        "#,
        "0",
        &[b"10", b"20", b"30"],
    );
    assert_eq!(result, RespFrame::Integer(60));
}

// ── String operations ───────────────────────────────

#[test]
fn lua_string_rep_bounded() {
    let mut rt = Runtime::default_strict();
    // string.rep with reasonable count should work
    let result = eval(&mut rt, r#"return string.rep('ab', 3)"#, "0", &[]);
    assert_eq!(result, RespFrame::BulkString(Some(b"ababab".to_vec())));
}

#[test]
fn lua_string_format_types() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"return string.format('%d %s %.1f', 42, 'hello', 3.14)"#,
        "0",
        &[],
    );
    assert_eq!(
        result,
        RespFrame::BulkString(Some(b"42 hello 3.1".to_vec()))
    );
}

// ── Table operations ────────────────────────────────

#[test]
fn lua_table_sort() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local t = {3, 1, 4, 1, 5, 9, 2, 6}
        table.sort(t)
        return {t[1], t[2], t[3], t[4]}
        "#,
        "0",
        &[],
    );
    assert_eq!(
        result,
        RespFrame::Array(Some(vec![
            RespFrame::Integer(1),
            RespFrame::Integer(1),
            RespFrame::Integer(2),
            RespFrame::Integer(3),
        ]))
    );
}

#[test]
fn lua_table_sort_custom_comparator() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local t = {3, 1, 4, 1, 5}
        table.sort(t, function(a, b) return a > b end)
        return {t[1], t[2], t[3]}
        "#,
        "0",
        &[],
    );
    assert_eq!(
        result,
        RespFrame::Array(Some(vec![
            RespFrame::Integer(5),
            RespFrame::Integer(4),
            RespFrame::Integer(3),
        ]))
    );
}

#[test]
fn lua_table_concat() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local t = {'a', 'b', 'c', 'd'}
        return table.concat(t, '-')
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::BulkString(Some(b"a-b-c-d".to_vec())));
}

// ── Error handling ──────────────────────────────────

#[test]
fn lua_error_function() {
    let mut rt = Runtime::default_strict();
    let result = eval(&mut rt, r#"error('custom error message')"#, "0", &[]);
    assert!(
        matches!(result, RespFrame::Error(_)),
        "error() should produce RESP error"
    );
}

#[test]
fn lua_pcall_catches_error() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local ok, err = pcall(error, 'boom')
        if ok then return 'should_not_happen' end
        return 'caught'
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::BulkString(Some(b"caught".to_vec())));
}

// ── Numeric edge cases ─────────────────────────────

#[test]
fn lua_integer_overflow_wraps() {
    let mut rt = Runtime::default_strict();
    // Large but valid computation
    let result = eval(&mut rt, r#"return 2^31 - 1"#, "0", &[]);
    assert_eq!(result, RespFrame::Integer(2_147_483_647));
}

#[test]
fn lua_float_to_integer_truncation() {
    let mut rt = Runtime::default_strict();
    let result = eval(&mut rt, r#"return math.floor(3.7)"#, "0", &[]);
    assert_eq!(result, RespFrame::Integer(3));
}

// ── Boolean handling ────────────────────────────────

#[test]
fn lua_boolean_true_returns_integer_1() {
    let mut rt = Runtime::default_strict();
    let result = eval(&mut rt, r#"return true"#, "0", &[]);
    assert_eq!(result, RespFrame::Integer(1));
}

#[test]
fn lua_boolean_false_returns_nil() {
    let mut rt = Runtime::default_strict();
    let result = eval(&mut rt, r#"return false"#, "0", &[]);
    assert_eq!(result, RespFrame::BulkString(None));
}

// ── Conditional logic ───────────────────────────────

#[test]
fn lua_ternary_pattern() {
    let mut rt = Runtime::default_strict();
    let result = eval(
        &mut rt,
        r#"
        local x = 10
        local result = x > 5 and 'big' or 'small'
        return result
        "#,
        "0",
        &[],
    );
    assert_eq!(result, RespFrame::BulkString(Some(b"big".to_vec())));
}

// ── Scripts and the selected database (frankenredis-ekwyb) ──
//
// The runtime applies SELECT by rewriting key positions in argv with
// `encode_db_key` before dispatch. Two things follow that a script can observe,
// and both were wrong:
//
//   1. EVAL's own key arguments are rewritten too, so the Lua KEYS table was
//      handed storage-encoded bytes. `#KEYS[1]` counted the 14-byte prefix and
//      `KEYS[1] == ARGV[1]` was false for one and the same key.
//   2. A key the SCRIPT names itself (a literal, or one built from ARGV) was
//      never in argv when the runtime rewrote it, so it reached the store
//      unprefixed -- i.e. in db 0 -- while the client sat on db 3.
//
// These drive `Runtime::execute_frame` on one client id rather than
// `dispatch_argv`, because the namespacing under test is the runtime's: a test
// that hand-encoded the key would be asserting against its own arithmetic.

fn select(rt: &mut Runtime, db: &[u8]) -> RespFrame {
    rt.execute_frame(command(&[b"SELECT", db]), 0)
}

fn get(rt: &mut Runtime, key: &[u8]) -> RespFrame {
    rt.execute_frame(command(&[b"GET", key]), 0)
}

#[test]
fn lua_keys_table_holds_the_logical_key_on_a_selected_db() {
    let mut rt = Runtime::default_strict();
    // db 0 is the identity encoding, so it is the control arm: what the script
    // sees here is what it must also see on db 3.
    assert_eq!(eval(&mut rt, r#"return #KEYS[1]"#, "1", &[b"mykey"]), RespFrame::Integer(5));

    assert_eq!(select(&mut rt, b"3"), RespFrame::SimpleString("OK".to_string()));
    assert_eq!(eval(&mut rt, r#"return #KEYS[1]"#, "1", &[b"mykey"]), RespFrame::Integer(5));
    assert_eq!(
        eval(&mut rt, r#"return KEYS[1]"#, "1", &[b"mykey"]),
        RespFrame::BulkString(Some(b"mykey".to_vec()))
    );
    // The comparison, not just the length: a prefix would make these differ.
    assert_eq!(
        eval(
            &mut rt,
            r#"if KEYS[1] == ARGV[1] then return 'same' else return 'DIFFERENT' end"#,
            "1",
            &[b"mykey", b"mykey"],
        ),
        RespFrame::BulkString(Some(b"same".to_vec()))
    );
    // And the leading bytes, which is where the encoding would show up first.
    assert_eq!(
        eval(&mut rt, r#"return string.sub(KEYS[1], 1, 2)"#, "1", &[b"mykey"]),
        RespFrame::BulkString(Some(b"my".to_vec()))
    );
}

#[test]
fn lua_script_named_key_lands_in_the_selected_db() {
    let mut rt = Runtime::default_strict();
    assert_eq!(select(&mut rt, b"3"), RespFrame::SimpleString("OK".to_string()));

    // A literal the script names itself, with no declared keys at all.
    eval(&mut rt, r#"redis.call('SET', 'litkey', 'v') return 1"#, "0", &[]);
    assert_eq!(get(&mut rt, b"litkey"), RespFrame::BulkString(Some(b"v".to_vec())));

    // A key the script builds out of ARGV, which is likewise never rewritten.
    eval(&mut rt, r#"redis.call('SET', ARGV[1], 'v') return 1"#, "0", &[b"argkey"]);
    assert_eq!(get(&mut rt, b"argkey"), RespFrame::BulkString(Some(b"v".to_vec())));

    // Neither may have leaked into db 0, which is where they used to go.
    assert_eq!(select(&mut rt, b"0"), RespFrame::SimpleString("OK".to_string()));
    assert_eq!(get(&mut rt, b"litkey"), RespFrame::BulkString(None));
    assert_eq!(get(&mut rt, b"argkey"), RespFrame::BulkString(None));
}

#[test]
fn lua_keys_derived_write_is_namespaced_exactly_once() {
    let mut rt = Runtime::default_strict();
    assert_eq!(select(&mut rt, b"3"), RespFrame::SimpleString("OK".to_string()));

    // Reading back through the same handle passes even when the key is encoded
    // twice, so the load-bearing assertion is the one OUTSIDE the script: a
    // double-prefixed key sits in db 3 under a name no client can reach.
    assert_eq!(
        eval(
            &mut rt,
            r#"redis.call('SET', KEYS[1], 'kv') return redis.call('GET', KEYS[1])"#,
            "1",
            &[b"mykey"],
        ),
        RespFrame::BulkString(Some(b"kv".to_vec()))
    );
    assert_eq!(get(&mut rt, b"mykey"), RespFrame::BulkString(Some(b"kv".to_vec())));

    assert_eq!(select(&mut rt, b"0"), RespFrame::SimpleString("OK".to_string()));
    assert_eq!(get(&mut rt, b"mykey"), RespFrame::BulkString(None));
}

#[test]
fn lua_db_zero_key_that_looks_encoded_is_left_alone() {
    // The strip keys off the SELECTED db, not off the byte pattern: on db 0
    // nothing was encoded, so a user key that merely starts with the namespace
    // prefix is an ordinary key and must survive verbatim.
    let mut rt = Runtime::default_strict();
    let mut odd_key = b"\x00frdb".to_vec();
    odd_key.extend_from_slice(&3_u64.to_be_bytes());
    odd_key.extend_from_slice(b"mykey");

    let len = rt.execute_frame(command(&[b"EVAL", b"return #KEYS[1]", b"1", &odd_key]), 0);
    assert_eq!(len, RespFrame::Integer(odd_key.len() as i64));

    eval(&mut rt, r#"redis.call('SET', KEYS[1], 'v') return 1"#, "1", &[&odd_key]);
    assert_eq!(get(&mut rt, &odd_key), RespFrame::BulkString(Some(b"v".to_vec())));
}

// ── Cross-database commands named by a script (frankenredis-mvcpy) ──
//
// MOVE and COPY are the one family that must NOT be namespaced on the way into
// dispatch_argv. They read dispatch_client_ctx.db_index and call encode_db_key
// themselves -- a cross-db command has to build a key for a database that is not
// the selected one -- so an already-encoded key gets double-prefixed and the
// transfer degrades to a silent `0`. They can hold that contract because a plain
// client never reaches them: the runtime intercepts both on the raw argv.

fn exists(rt: &mut Runtime, db: &[u8], key: &[u8]) -> RespFrame {
    select(rt, db);
    rt.execute_frame(command(&[b"EXISTS", key]), 0)
}

#[test]
fn lua_script_move_transfers_out_of_the_selected_db() {
    for src in [&b"0"[..], &b"3"[..]] {
        let mut rt = Runtime::default_strict();
        assert_eq!(select(&mut rt, src), RespFrame::SimpleString("OK".to_string()));
        rt.execute_frame(command(&[b"SET", b"mk", b"v"]), 0);

        assert_eq!(
            eval(&mut rt, r#"return redis.call('MOVE','mk',7)"#, "0", &[]),
            RespFrame::Integer(1),
            "script MOVE from db {}",
            String::from_utf8_lossy(src)
        );
        assert_eq!(exists(&mut rt, b"7", b"mk"), RespFrame::Integer(1));
        assert_eq!(exists(&mut rt, src, b"mk"), RespFrame::Integer(0));
    }
}

#[test]
fn lua_script_move_transfers_a_declared_key() {
    // KEYS[1] now holds the LOGICAL key, so it must reach MOVE unprefixed just
    // like a literal does. This arm was broken independently of the namespacing:
    // before KEYS was made logical it carried encoded bytes that MOVE re-encoded.
    for src in [&b"0"[..], &b"3"[..]] {
        let mut rt = Runtime::default_strict();
        assert_eq!(select(&mut rt, src), RespFrame::SimpleString("OK".to_string()));
        rt.execute_frame(command(&[b"SET", b"mk", b"v"]), 0);

        assert_eq!(
            eval(&mut rt, r#"return redis.call('MOVE',KEYS[1],7)"#, "1", &[b"mk"]),
            RespFrame::Integer(1),
            "script MOVE via KEYS from db {}",
            String::from_utf8_lossy(src)
        );
        assert_eq!(exists(&mut rt, b"7", b"mk"), RespFrame::Integer(1));
        assert_eq!(exists(&mut rt, src, b"mk"), RespFrame::Integer(0));
    }
}

#[test]
fn lua_script_copy_reaches_another_db_and_leaves_the_source() {
    for src in [&b"0"[..], &b"3"[..]] {
        let mut rt = Runtime::default_strict();
        assert_eq!(select(&mut rt, src), RespFrame::SimpleString("OK".to_string()));
        rt.execute_frame(command(&[b"SET", b"mk", b"v"]), 0);

        assert_eq!(
            eval(&mut rt, r#"return redis.call('COPY','mk','mk2','DB',7)"#, "0", &[]),
            RespFrame::Integer(1),
            "script COPY from db {}",
            String::from_utf8_lossy(src)
        );
        assert_eq!(exists(&mut rt, b"7", b"mk2"), RespFrame::Integer(1));
        // COPY is not MOVE: the source must survive in the selected db.
        assert_eq!(exists(&mut rt, src, b"mk"), RespFrame::Integer(1));
    }
}

#[test]
fn debug_is_refused_from_a_script_so_it_needs_no_logical_key_exemption() {
    // debug_cmd resolves its key the same way MOVE and COPY do, and is deliberately
    // absent from command_resolves_keys_against_selected_db. That is only sound while
    // no script route can reach it -- upstream refuses DEBUG from a script and so does
    // fr. If this ever starts succeeding, DEBUG needs adding to the exemption.
    let mut rt = Runtime::default_strict();
    assert_eq!(select(&mut rt, b"3"), RespFrame::SimpleString("OK".to_string()));
    rt.execute_frame(command(&[b"SET", b"mk", b"v"]), 0);

    let reply = eval(&mut rt, r#"return redis.call('DEBUG','OBJECT','mk')"#, "0", &[]);
    let RespFrame::Error(message) = reply else {
        panic!("DEBUG must be refused from a script, got {reply:?}"); // ubs:ignore — AI triage
    };
    assert!(
        message.contains("not allowed from script"),
        "unexpected refusal message: {message}"
    );
}

// ── Bindings that return NOTHING, not nil (frankenredis-luavoid) ──
//
// Lua distinguishes "no values" from "one nil value", and upstream's C bindings
// use both: `redis.call` ends in `return 1`, while `luaLogCommand`, `luaSetResp`,
// `luaRedisSetReplCommand`, `luaRedisDebugCommand` and `lmathlib.c::math_randomseed`
// all end in `return 0`. fr returned a nil from each of those five, which is
// invisible to `tostring()` and to a table constructor but not to `select('#')`,
// nor to a trailing call in a multiple assignment.
//
// upstream's own comment on redis.debug says it outright: "Nothing is returned to
// the caller".

/// `select('#', <call>)` -- the number of values a binding actually returns.
fn returned_value_count(rt: &mut Runtime, call: &str) -> RespFrame {
    let script = format!("return select('#', {call})");
    eval(rt, &script, "0", &[])
}

#[test]
fn lua_void_bindings_return_no_values_at_all() {
    let mut rt = Runtime::default_strict();
    for call in [
        "redis.log(redis.LOG_WARNING, 'x')",
        "redis.setresp(2)",
        "redis.set_repl(redis.REPL_ALL)",
        "redis.debug('x')",
        "redis.debug()",
        "redis.debug('a', 'b')",
        "math.randomseed(1)",
    ] {
        assert_eq!(
            returned_value_count(&mut rt, call),
            RespFrame::Integer(0),
            "{call} must return no values",
        );
    }
}

#[test]
fn lua_value_returning_bindings_still_return_exactly_one() {
    // The anti-vacuity half. Without it, an implementation that returned nothing from
    // EVERY binding would satisfy the test above while breaking redis.call entirely.
    let mut rt = Runtime::default_strict();
    for call in [
        "redis.call('PING')",
        "redis.pcall('PING')",
        "redis.replicate_commands()",
        "redis.breakpoint()",
        "redis.sha1hex('')",
        "redis.status_reply('x')",
        "redis.error_reply('x')",
        "redis.acl_check_cmd('GET', 'k')",
        "math.random()",
    ] {
        assert_eq!(
            returned_value_count(&mut rt, call),
            RespFrame::Integer(1),
            "{call} must return exactly one value",
        );
    }
}

#[test]
fn lua_void_bindings_still_do_their_work() {
    // Returning nothing must not turn them into no-ops: each still has to have its
    // side effect, or "parity" would have been bought by deleting the feature.
    let mut rt = Runtime::default_strict();

    // setresp(3) still switches redis.call's conversion to the RESP3 shapes. HGETALL, not
    // CONFIG GET: CONFIG is `noscript` upstream, so a script cannot call it at all and the
    // assertion would pass on two identical refusals without ever reaching a map.
    assert_eq!(
        eval(
            &mut rt,
            r#"redis.call('HSET', 'h', 'f', 'v')
               redis.setresp(3)
               local r = redis.call('HGETALL', 'h')
               return type(r) .. ',' .. tostring(r.map ~= nil) .. ',' .. tostring(r.map.f)"#,
            "0",
            &[],
        ),
        RespFrame::BulkString(Some(b"table,true,v".to_vec()))
    );
    // RESP2 is still the default shape, so the switch above was real and not the baseline.
    assert_eq!(
        eval(
            &mut rt,
            r#"redis.call('HSET', 'h2', 'f', 'v')
               local r = redis.call('HGETALL', 'h2')
               return type(r) .. ',' .. tostring(r.map ~= nil) .. ',' .. tostring(#r)"#,
            "0",
            &[],
        ),
        RespFrame::BulkString(Some(b"table,false,2".to_vec()))
    );

    // randomseed still seeds: the same seed must reproduce the same draw.
    assert_eq!(
        eval(
            &mut rt,
            r#"math.randomseed(1) local a = math.random()
               math.randomseed(1) local b = math.random()
               return tostring(a == b)"#,
            "0",
            &[],
        ),
        RespFrame::BulkString(Some(b"true".to_vec()))
    );

    // set_repl still accepts its flags and leaves the script able to write.
    assert_eq!(
        eval(
            &mut rt,
            r#"redis.set_repl(redis.REPL_NONE)
               redis.call('SET', 'k', 'v')
               redis.set_repl(redis.REPL_ALL)
               return redis.call('GET', 'k')"#,
            "0",
            &[],
        ),
        RespFrame::BulkString(Some(b"v".to_vec()))
    );

    // And the argument validation each one had is untouched.
    let bad = eval(&mut rt, r#"return redis.setresp(4)"#, "0", &[]);
    let RespFrame::Error(message) = bad else {
        panic!("redis.setresp(4) must be an error, got {bad:?}"); // ubs:ignore — AI triage
    };
    assert!(message.contains("RESP version must be 2 or 3"), "got {message}");
}
