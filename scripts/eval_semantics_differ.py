#!/usr/bin/env python3
"""eval_semantics_differ.py — EVAL/Lua core-language + RESP-conversion differential vs redis 7.2.4.

fr runs a CUSTOM, GC-less Lua interpreter (fr-command/lua_eval.rs) rather than
embedding real Lua, so it's the single highest-risk parity surface — it already
leaked a per-EVAL Rc-cycle DoS (qqq17). lua_lib_differ.py exercises the Lua
STDLIB functions (cjson/cmsgpack/struct/bit/redis.*); this gate instead nails
the LANGUAGE CORE and the Lua<->RESP conversion that every script depends on:
arithmetic + numeric->integer truncation, boolean/nil/string coercion, table
constructors (incl. nil holes and nested tables), control flow, comparisons,
multiple returns, {ok=}/{err=} status-and-error replies, redis.call argument
coercion, and pcall/error semantics.

All ~165 scripts below are byte-exact vs vendored 7.2.4 (PASS) — the gate guards
against a regression in the interpreter's evaluation or reply conversion.

SETUP (oracle config-less => compiled defaults; fr strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    cargo build -p fr-server          # CARGO_TARGET_DIR=/data/tmp/cargo-target here
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/eval_semantics_differ.py 16399 16400

NON-DETERMINISM: a script that returns a bare table/function reaches the client
as `table: 0x<addr>` / `function: 0x<addr>` whose pointer differs per process —
those are normalized away (not fr bugs).
"""
import socket
import sys
import re

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400

# Each entry is a Lua body for `EVAL <body> 1 k1 a1`.
SCRIPTS = [
    # literals / RESP scalar conversion
    "return 1", "return 1.5", "return 'hello'", "return true", "return false",
    "return nil", "return 0", "return -1", "return ''", "return 3.0", "return 3.999",
    "return -3.999", "return 0.0", "return 100000000000",
    # numeric -> RESP integer truncation (Lua numbers are doubles; redis truncates)
    "return 3.99", "return -3.99", "return 2.5", "return 1e10",
    "return 9007199254740993", "return 2^53", "return 2^53+1", "return 1/3*3",
    # arithmetic / logic
    "return 10/3", "return 7%3", "return 2^10", "return -5", "return 1==1",
    "return 1~=2", "return 1 and 2", "return nil and 2", "return 1 or 2",
    "return false or 'x'", "return not nil", "return 5 < 10", "return 'a' < 'b'",
    "return {} == {}", "return nil == false", "return true == 1",
    # tables (incl. nil holes, nesting) -> multibulk conversion stops at first nil
    "return {1,2,3}", "return {1,'two',3}", "return {1,2,nil,4}", "return {}",
    "return {true,false}", "return {nil}", "return {1,2,3,nil,5}",
    "return {[1]=1,[2]=2,[4]=4}", "return {1, {2, {3}}}", "return {1,2,3,'ciao',{1,2}}",
    # status / error replies
    "return {ok='mystatus'}", "return {err='myerror'}", "return {ok='OK', extra='ignored'}",
    "return redis.status_reply('GOOD')", "return redis.error_reply('bad')",
    "return redis.error_reply('no prefix here')", "return redis.status_reply('PONG').ok",
    "return redis.error_reply('E').err",
    # redis.call / pcall and argument coercion
    "return redis.call('SET',KEYS[1],ARGV[1])", "return redis.call('GET',KEYS[1])",
    "return redis.call('INCR','counter')", "return redis.call('LPUSH','mylist','a','b')",
    "return redis.call('GET','nonexistent')", "return redis.call('SET','n',5)",
    "redis.call('SET','n',5); return redis.call('GET','n')",
    "redis.call('SET','n',3.7); return redis.call('GET','n')",
    "return redis.call('EXISTS','nope')", "redis.call('SET','n','1'); return redis.call('EXISTS','n')==1",
    "return redis.pcall('INCR','n','x')", "return redis.pcall('INCR','n','x').err",
    "local ok,e=pcall(redis.call,'INCR','n','x'); return {tostring(ok), type(e)}",
    # error()
    "error('custom')", "error({err='tablerr'})", "error('x', 0)",
    "local ok,err = pcall(function() error('boom') end); return tostring(ok)",
    # type/tostring/tonumber
    "return type({})", "return type(1)", "return type('s')", "return tostring(nil)",
    "return tostring(true)", "return tostring(1.5)", "return tostring(0.1)",
    "return tonumber('42')", "return tonumber('  42  ')", "return tonumber('')",
    "return tonumber('abc')", "return tonumber('0x1A')", "return tonumber('1e3')",
    # control flow / iteration
    "if 1==1 then return 'yes' else return 'no' end",
    "local x=0; for i=1,5 do x=x+i end; return x",
    "local t={}; for i=1,3 do t[i]=i*i end; return t",
    "local t={10,20,30}; local s=0; for i,v in ipairs(t) do s=s+v end; return s",
    "local n=0; for k,v in pairs({a=1,b=2}) do n=n+1 end; return n",
    # multiple returns / select / indexing
    "return select('#', 1,2,3)", "return select(2, 'a','b','c')",
    "local function f() return 1,2,3 end; return {f()}", "return ({1,2,3})[2]",
    # string library core
    "return #'hello'", "return string.rep('a',5)", "return string.rep('ab',0)",
    "return string.sub('hello',2,4)", "return string.sub('hello',-3)",
    "return string.sub('hello',-3,-1)", "return string.sub('hello',0)",
    "return string.len('abc')", "return string.upper('ab')", "return string.lower('AB')",
    "return ('hi'):upper()", "return string.find('hello','l')",
    "return string.find('hello world','o',6)", "return ({string.find('aXbXc','X')})[1]",
    "return string.gsub('aaa','a','b')", "return string.byte('A')",
    "return string.char(66,67)", "return string.format('%d-%s',5,'x')",
    "return string.format('%.2f', 3.14159)", "return string.format('%x', 255)",
    "return string.format('%c',65)", "return string.format('%5.2f',3.14159)",
    "return #''", "return #'\\0\\0\\0'", "return string.char(0,255,128)",
    # math library
    "return math.floor(3.7)", "return math.floor(-3.5)", "return math.ceil(-3.5)",
    "return math.abs(-5)", "return math.fmod(7,3)", "return math.sqrt(16)",
    "return math.max(1,5,3)", "return math.min(1,5,3)", "return math.huge",
    # KEYS / ARGV
    "return ARGV[1]", "return KEYS[1]", "return #KEYS", "return #ARGV",
    "return redis.sha1hex('')", "return redis.sha1hex('abc')",
    # table.remove / length
    "return table.remove({1,2,3})", "return #({1,2,3})",
]

# Comparing-error scripts (Lua raises on number<string etc.) — both must error.
ERROR_SCRIPTS = [
    "return 1 < 'a'", "return redis.call('SET','n',true)",
    "return redis.call('INCR','n','extra')", "return {err=1}",
]

_ADDR = re.compile(rb"(table|function): 0x[0-9a-f]+")


def _read_reply(s):
    data = bytearray()

    def read_line():
        line = bytearray()
        while not line.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            line += ch
        return bytes(line)

    def one():
        line = read_line()
        data.extend(line)
        if not line:
            return
        t = line[:1]
        if t in (b"+", b"-", b":", b"_", b"#", b",", b"("):
            return
        if t in (b"$", b"="):
            n = int(line[1:-2])
            if n < 0:
                return
            body = b""
            while len(body) < n + 2:
                body += s.recv(n + 2 - len(body))
            data.extend(body)
            return
        if t in (b"*", b"~", b">", b"%"):
            n = int(line[1:-2])
            if n < 0:
                return
            if t == b"%":
                n *= 2
            for _ in range(n):
                one()

    one()
    return bytes(data)


def send(s, *args):
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return _read_reply(s)


def _norm(reply):
    # Bare table/function -> `table: 0x<addr>`; the pointer is process-specific.
    return _ADDR.sub(rb"\1: 0xPTR", reply)


def _normalize_error(reply):
    # Both should be errors; exact wording for raised Lua errors carries a script
    # line/source tag that differs, so for the ERROR_SCRIPTS only assert both are
    # error frames (start with '-').
    return reply[:1] == b"-"


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else ORACLE_DEFAULT
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else FR_DEFAULT
    o = socket.create_connection(("127.0.0.1", op))
    f = socket.create_connection(("127.0.0.1", fp))
    o.settimeout(3)
    f.settimeout(3)
    send(o, "FLUSHALL")
    send(f, "FLUSHALL")
    div = 0
    for sc in SCRIPTS:
        ro = _norm(send(o, "EVAL", sc, "1", "k1", "a1"))
        rf = _norm(send(f, "EVAL", sc, "1", "k1", "a1"))
        if ro != rf:
            div += 1
            print(f"DIVERGE {sc!r}\n  oracle: {ro!r}\n  fr    : {rf!r}")
    for sc in ERROR_SCRIPTS:
        eo = _normalize_error(send(o, "EVAL", sc, "1", "k1", "a1"))
        ef = _normalize_error(send(f, "EVAL", sc, "1", "k1", "a1"))
        if eo != ef:
            div += 1
            print(f"DIVERGE(error-ness) {sc!r}: oracle_is_error={eo} fr_is_error={ef}")
    total = len(SCRIPTS) + len(ERROR_SCRIPTS)
    print("-" * 60)
    print(f"checked {total} EVAL scripts; divergences: {div}")
    if div == 0:
        print("PASS — fr EVAL/Lua core semantics match redis 7.2.4")
        return 0
    print(f"FAIL — {div} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
