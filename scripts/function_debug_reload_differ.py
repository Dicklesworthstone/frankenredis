#!/usr/bin/env python3
"""Differential gate: FUNCTION libraries must survive DEBUG RELOAD (frankenredis-i0yd6).

`DEBUG RELOAD` round-trips the dataset through an RDB. Libraries loaded with
FUNCTION LOAD are carried in the dump as RDB_OPCODE_FUNCTION2 records, so redis
answers FUNCTION LIST / FCALL identically before and after. fr lost every library
across the reload whenever an RDB path was configured -- which is the default,
since fr-server falls back to `dump.rdb` so SAVE/BGSAVE are not silent no-ops --
because the reload's RDB-file branch read with a decoder that dropped FUNCTION2.
Fixed in 9e7799cef; this gate is why it cannot come back.

WHY THIS GATE EXISTS SEPARATELY (frankenredis-hj7d6). The bug was found while
diagnosing resp3_reply_type_gate, and that gate was assumed to cover it. It does
not: with the fix deleted, resp3_reply_type_gate still passes all 36 probes,
because its original divergence was a polluted-oracle-cwd artifact (fixed under
frankenredis-1zpr7), not this bug. Verified by mutation. Until now i0yd6 had unit
coverage only.

THE ASSERTION THAT MATTERS MOST IS THAT THE RELOAD RAN. `DEBUG` is refused by
default on both engines ("DEBUG command not allowed... enable-debug-command"),
and a probe that discards that error sees libraries "survive" a reload that never
happened -- a false PASS that made a deliberately-broken build look correct while
this gate was being written. Every DEBUG RELOAD below is asserted to return OK.
parity_suite.py launches both servers with --enable-debug-command yes.

Usage: function_debug_reload_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import sys

from _respread import assert_ok, assert_seed, cmd, conn

LIB_ONE = (
    "#!lua name=lib_one\n"
    "redis.register_function('fn_a', function() return 1 end)\n"
)
LIB_TWO = (
    "#!lua name=lib_two\n"
    "redis.register_function('fn_b', function() return 'b' end)\n"
    "redis.register_function('fn_c', function(keys, args) return #args end)\n"
)

# FCALL replies ARE fully specified, so these are compared byte-exact. They are
# the sharpest probe available: a library can be listed while its functions are
# unregistered, and only an actual call proves the function is callable.
PROBES = [
    ["FCALL", "fn_a", "0"],
    ["FCALL", "fn_b", "0"],
    ["FCALL", "fn_c", "0", "x", "y"],
]

# FUNCTION LIST is NOT compared byte-exact, and neither is FUNCTION DUMP. Both
# engines order libraries -- and the functions within a library -- however their
# internal map iterates: on the very same corpus redis returned lib_two before
# lib_one while fr returned lib_one before lib_two, and FUNCTION DUMP serialises
# in that same unspecified order, so its bytes differ for two engines holding
# identical libraries. A byte-exact compare here fails forever for a reason that
# has nothing to do with the bug under test -- the mistake frankenredis-z7fa2
# recorded for SCAN. NARROWED, not dropped: the reply is decoded and compared as
# {library -> sorted(function names)}, which still catches a missing library, a
# missing function, an extra one, or the whole set vanishing (the i0yd6 symptom,
# where fr answers a bare `*0`).


def resp_decode(buf, i=0):
    """Minimal RESP2 decoder: returns (value, next_index)."""
    kind = buf[i : i + 1]
    end = buf.index(b"\r\n", i)
    head = buf[i + 1 : end]
    if kind in (b"+", b"-", b":"):
        return head, end + 2
    if kind == b"$":
        n = int(head)
        if n == -1:
            return None, end + 2
        return buf[end + 2 : end + 2 + n], end + 2 + n + 2
    if kind == b"*":
        n = int(head)
        if n == -1:
            return None, end + 2
        out, j = [], end + 2
        for _ in range(n):
            v, j = resp_decode(buf, j)
            out.append(v)
        return out, j
    raise ValueError(f"unexpected RESP byte {kind!r}")


def function_map(reply):
    """{library_name: sorted[function names]} from a FUNCTION LIST reply."""
    libs, _ = resp_decode(reply)
    out = {}
    for lib in libs or []:
        d = {lib[k]: lib[k + 1] for k in range(0, len(lib) - 1, 2)}
        names = sorted(
            {f[k + 1] for f in d.get(b"functions", []) for k in range(0, len(f) - 1, 2)
             if f[k] == b"name"}
        )
        out[d.get(b"library_name")] = names
    return out


def assert_bulk(reply, expected, label):
    """`assert_seed`'s bulk-string companion: FUNCTION LOAD answers the library
    name, not an integer, and a silently failed load would leave the gate
    comparing two engines that both have no libraries -- passing having tested
    nothing, which is exactly the failure class these asserts exist for."""
    want = b"$%d\r\n%s\r\n" % (len(expected), expected.encode())
    if reply != want:
        print(f"SEED FAILED [{label}]: got {reply!r}, expected {want!r}")
        sys.exit(1)


def seed(sock):
    assert_ok(cmd(sock, "FLUSHALL"), "FLUSHALL")
    assert_ok(cmd(sock, "FUNCTION", "FLUSH"), "FUNCTION FLUSH")
    assert_bulk(cmd(sock, "FUNCTION", "LOAD", LIB_ONE), "lib_one", "FUNCTION LOAD lib_one")
    assert_bulk(cmd(sock, "FUNCTION", "LOAD", LIB_TWO), "lib_two", "FUNCTION LOAD lib_two")
    # A plain key, so a reload that drops the whole dataset is distinguishable
    # from one that drops only the libraries.
    assert_ok(cmd(sock, "SET", "plain", "v"), "SET plain")
    assert_seed(cmd(sock, "FCALL", "fn_a", "0"), 1, "FCALL fn_a before reload")


def compare(od, fr, label, fails):
    for argv in PROBES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"[{label}] {' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    ro, rf = cmd(od, "GET", "plain"), cmd(fr, "GET", "plain")
    if ro != rf:
        fails.append(f"[{label}] 'GET plain': redis={ro!r} fr={rf!r}")
    mo = function_map(cmd(od, "FUNCTION", "LIST"))
    mf = function_map(cmd(fr, "FUNCTION", "LIST"))
    if mo != mf:
        fails.append(f"[{label}] FUNCTION LIST libraries: redis={mo!r} fr={mf!r}")
    for lib in ("lib_one", "lib_two"):
        mo = function_map(cmd(od, "FUNCTION", "LIST", "LIBRARYNAME", lib))
        mf = function_map(cmd(fr, "FUNCTION", "LIST", "LIBRARYNAME", lib))
        if mo != mf:
            fails.append(f"[{label}] FUNCTION LIST LIBRARYNAME {lib}: redis={mo!r} fr={mf!r}")


def reload_both(od, fr):
    """DEBUG RELOAD on both, asserting each actually ran."""
    for sock, who in ((od, "redis"), (fr, "fr")):
        reply = cmd(sock, "DEBUG", "RELOAD")
        if reply != b"+OK\r\n":
            print(
                f"VOID — {who} refused DEBUG RELOAD ({reply!r}). Start both servers with "
                f"--enable-debug-command yes; without it this gate would pass without "
                f"reloading anything."
            )
            sys.exit(2)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        seed(s)

    fails = []
    compare(od, fr, "before reload", fails)
    reload_both(od, fr)
    compare(od, fr, "after 1st reload", fails)
    # A second reload catches a restore that works once but not from a dump the
    # engine itself just wrote.
    reload_both(od, fr)
    compare(od, fr, "after 2nd reload", fails)

    # FUNCTION FLUSH must still take effect across a reload, so a gate that only
    # ever adds libraries cannot pass by never clearing them.
    for s in (od, fr):
        assert_ok(cmd(s, "FUNCTION", "FLUSH"), "FUNCTION FLUSH post-reload")
    reload_both(od, fr)
    mo = function_map(cmd(od, "FUNCTION", "LIST"))
    mf = function_map(cmd(fr, "FUNCTION", "LIST"))
    if mo != mf or mf:
        fails.append(f"[after flush+reload] FUNCTION LIST: redis={mo!r} fr={mf!r} (both must be empty)")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} FUNCTION/DEBUG RELOAD divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — FUNCTION libraries survive DEBUG RELOAD identically to redis 7.2.4 "
        f"({len(PROBES)} FCALL probes byte-exact + library/function sets, at 3 reload "
        "points, 2 libraries / 3 functions, plus flush-then-reload)"
    )


if __name__ == "__main__":
    main()
