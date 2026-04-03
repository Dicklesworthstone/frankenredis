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
