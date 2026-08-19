#!/usr/bin/env python3
"""Diff Lua language semantics between fr and the incumbent, one invocation, reply by reply.

Grew out of frankenredis-lua-tail-calls-ps0le. That defect was not a wrong LIMIT, it was a violated
LANGUAGE GUARANTEE -- Lua 5.1 specifies proper tail calls and fr charged stack for them -- and it was
found by accident while probing something else. This asks the question deliberately and in bulk:
for a battery of small scripts whose behaviour the Lua manual pins down, do the two engines answer
the same thing?

Every case is a one-liner whose reply is small and total, so a divergence is a STRING difference
rather than a judgement call. Cases that are expected to error are included on purpose: the error
TEXT is as much a parity surface as the value, and this project has already found several wordings
that no test pinned.

Usage:  lua_semantics_differential.py [substring-filter]
Exit 0 when every case matches, 1 otherwise.
"""
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

ROOT = "/data/projects/frankenredis"
FR = os.path.join(ROOT, "target/release/frankenredis")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
FR_PORT, REDIS_PORT = 7781, 7782

# (name, script). Keep each reply short: this is a semantics diff, not a payload test.
CASES = [
    # --- numbers and their printed form: Lua 5.1 has one number type, and Redis converts a Lua
    # --- number to an INTEGER reply by truncation, so both the maths and the truncation show here.
    ("num_int_tostring",      "return tostring(1)"),
    ("num_float_tostring",    "return tostring(1.5)"),
    ("num_intlike_tostring",  "return tostring(3.0)"),
    ("num_div_is_float",      "return tostring(7/2)"),
    ("num_truncation",        "return 3.7"),
    ("num_negative_trunc",    "return -3.7"),
    ("num_big",               "return tostring(2^53)"),
    ("num_mod_negative",      "return tostring(-5 % 3)"),
    ("num_pow",               "return tostring(2^10)"),
    ("num_scientific",        "return tostring(1e15)"),
    ("num_tonumber_hex",      "return tostring(tonumber('0x10'))"),
    ("num_tonumber_base",     "return tostring(tonumber('ff', 16))"),
    ("num_tonumber_junk",     "return tostring(tonumber('12abc'))"),
    ("num_string_coerce",     "return tostring('10' + 5)"),
    # --- string library
    ("str_format_d",          "return string.format('%d', 42)"),
    ("str_format_g",          "return string.format('%g', 0.1)"),
    ("str_format_q",          "return string.format('%q', 'a\"b')"),
    ("str_rep_zero",          "return '[' .. string.rep('x', 0) .. ']'"),
    ("str_sub_negative",      "return ('hello'):sub(-3)"),
    ("str_sub_zero",          "return ('hello'):sub(0, 2)"),
    ("str_byte_multi",        "return tostring(select('#', ('abc'):byte(1, -1)))"),
    ("str_find_plain",        "return tostring(('a.b'):find('.', 1, true))"),
    ("str_gsub_count",        "return tostring(select(2, ('aaa'):gsub('a', 'b')))"),
    ("str_upper_locale",      "return ('aeiou'):upper()"),
    # --- tables
    ("tbl_len_hole",          "local t = {1, nil, 3} return tostring(#t == 1 or #t == 3)"),
    ("tbl_concat",            "return table.concat({1, 2, 3}, '-')"),
    ("tbl_sort_cmp",          "local t = {3, 1, 2} table.sort(t) return table.concat(t, ',')"),
    ("tbl_remove_end",        "local t = {1, 2, 3} table.remove(t) return table.concat(t, ',')"),
    ("tbl_insert_pos",        "local t = {1, 3} table.insert(t, 2, 2) return table.concat(t, ',')"),
    ("tbl_unpack",            "return tostring(select('#', unpack({1, 2, 3})))"),
    # --- language guarantees
    ("varargs_count",         "local function f(...) return select('#', ...) end return f(1, nil, 3)"),
    ("varargs_nil_tail",      "local function f(...) return select('#', ...) end return f(nil, nil)"),
    ("closure_upvalue",       "local c = 0 local function f() c = c + 1 return c end f() f() return f()"),
    ("metatable_index",       "local t = setmetatable({}, {__index = function() return 7 end}) return t.anything"),
    ("metatable_call",        "local t = setmetatable({}, {__call = function() return 9 end}) return t()"),
    ("pcall_returns_false",   "local ok = pcall(function() error('x') end) return tostring(ok)"),
    ("pcall_error_value",     "local _, e = pcall(function() error({code = 5}) end) return tostring(type(e))"),
    ("error_level_zero",      "local _, e = pcall(function() error('bare', 0) end) return e"),
    ("select_negative",       "return tostring(select(-1, 'a', 'b', 'c'))"),
    ("coroutine_resume",      "local co = coroutine.create(function() coroutine.yield(1) return 2 end) "
                              "local _, a = coroutine.resume(co) local _, b = coroutine.resume(co) "
                              "return tostring(a) .. ',' .. tostring(b)"),
    # --- errors whose TEXT is the parity surface
    ("err_arith_on_string",   "local _, e = pcall(function() return {} + 1 end) return e"),
    ("err_index_nil",         "local _, e = pcall(function() local x return x.y end) return e"),
    ("err_call_nil",          "local _, e = pcall(function() local f f() end) return e"),
    ("err_concat_table",      "local _, e = pcall(function() return {} .. 'x' end) return e"),
    ("err_compare_mixed",     "local _, e = pcall(function() return 1 < 'a' end) return e"),
    ("err_bad_argument",      "local _, e = pcall(function() return ('x'):rep('a') end) return e"),
    # The METHOD form is the one that diverges: upstream's luaL_argerror decrements the reported
    # index when the call is a method so `self` is not counted (lauxlib.c). The plain form is here
    # as the control -- it already matches, which is what localises the defect to the method path.
    ("argerr_method_sub",     "local _, e = pcall(function() return ('x'):sub('a') end) return e"),
    ("argerr_plain_control",  "local _, e = pcall(function() return string.rep('x', 'a') end) return e"),
    # The string metatable is reachable in 7.2.4; extending it is a known Lua idiom.
    ("string_metatable",      "return type(getmetatable(''))"),
    ("string_mt_index",       "local m = getmetatable('') return tostring(m ~= nil and m.__index ~= nil)"),
]


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


def start(binary, port, tag, env_extra=None):
    d = tempfile.mkdtemp(prefix=f"luasem_{tag}_")
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    p = subprocess.Popen([binary, "--port", str(port), "--dir", d, "--save", ""],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    for _ in range(120):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return p
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"{tag} did not start on {port}")


def resp(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        b = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def call(port, *args):
    try:
        s = socket.create_connection(("127.0.0.1", port), 15)
        s.settimeout(15)
        s.sendall(resp(*args))
        buf = b""
        while not buf.endswith(b"\r\n"):
            c = s.recv(65536)
            if not c:
                break
            buf += c
        s.close()
        return buf.decode(errors="replace").strip()
    except (OSError, socket.timeout) as e:
        return f"<no reply: {type(e).__name__}>"


def normalise(t):
    """Mask the parts that legitimately differ between two servers running the same script."""
    return re.sub(r"[0-9a-f]{40}", "<sha>", t)


def main():
    filt = sys.argv[1] if len(sys.argv) > 1 else ""
    fr = start(FR, FR_PORT, "fr", {"FR_SHARED_NOTHING_PARTITIONS": "4"})
    rd = start(REDIS, REDIS_PORT, "redis")
    diverged = []
    try:
        print("lua semantics differential -- fr vs redis 7.2.4, loadavg %s" % loadavg())
        print()
        for name, script in CASES:
            if filt and filt not in name:
                continue
            a = normalise(call(FR_PORT, "EVAL", script, "0"))
            b = normalise(call(REDIS_PORT, "EVAL", script, "0"))
            if a == b:
                continue
            diverged.append((name, a, b))
            print("DIVERGE  %-22s" % name)
            print("   fr    %s" % a[:150])
            print("   redis %s" % b[:150])
        total = len([c for c in CASES if not filt or filt in c[0]])
        print()
        print("%d cases, %d diverged, loadavg %s" % (total, len(diverged), loadavg()))
    finally:
        for p in (fr, rd):
            p.terminate()
        for p in (fr, rd):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
    return 1 if diverged else 0


if __name__ == "__main__":
    sys.exit(main())
