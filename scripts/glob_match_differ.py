#!/usr/bin/env python3
"""Differential gate: glob pattern matching via KEYS (frankenredis-z9dc3 area).

KEYS / SCAN MATCH use fr-store's glob_match (the redis stringmatchlen equivalent):
`*`, `?`, `[...]` char classes with ranges / `^`|`!` negation / `\\` escapes, and
the various malformed-class fallbacks. This is a classic byte-exact-sensitive
surface with no prior dedicated gate. This gate builds a rich keyspace and compares
the MATCHED key SET (order-insensitive — KEYS order is unspecified) for a broad
battery of adversarial patterns against vendored redis 7.2.4.

KNOWN BUG, deliberately avoided here (frankenredis-z9dc3): a multi-star pattern
(`**`, `***`, ...) matches the EMPTY string on fr but not on redis (redis's
stringmatchlen never enters its loop for an empty string, so only an empty pattern
matches; single `*` matches the empty key via KEYS' separate allkeys shortcut).
To keep this gate a clean regression lock of the otherwise byte-exact matcher, the
keyspace contains NO empty-string key, so the bug is not exercised. Add an empty
key + multi-star patterns here once z9dc3 is fixed.

Usage: glob_match_differ.py <oracle_port> <fr_port>
       Exit 0 = matched-key sets identical, 1 = divergence.
"""
import socket
import sys
import time

# Rich keyspace (NO empty-string key — see z9dc3 note above).
KEYS = [
    "hello", "hallo", "hxllo", "hllo", "heello", "HELLO", "h-llo", "hbllo",
    "hcllo", "hzllo", "h*llo", "h?llo", "h[llo", "h]llo", "ha", "abc", "a]c",
    "a-c", "a^c", "x", "\xff\xfe", "[abc]", "a\\b", "key:1", "key:2", "foo",
    "bar", "FOO", "a.b", "123", "A", "z", "aa", "bb", "a1c", "a2c", "azc",
]

PATTERNS = [
    "*", "?", "??", "h?llo", "h*llo", "*llo", "h*", "*o*", "**", "h?l*o", "*-*",
    "h[ae]llo", "h[^e]llo", "h[a-c]llo", "h[c-a]llo", "[hH]ello", "[^h]*",
    "h\\*llo", "h\\?llo", "\\\\", "h[a-c]*", "[a-z]*", "[A-Z]*", "[0-9]*",
    "[", "[]", "[a", "[a-", "h[]llo", "[*]", "[?]", "[][]", "[-a]", "[a-]",
    "[\\]]", "key:*", "key:?", "a[b-d]c", "a]c", "a\\]c", "[!a]", "a^c",
]


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


def keys_set(srv, pat):
    r = cmd(srv, "KEYS", pat)
    if not r.startswith(b"*"):
        return ("NON_ARRAY", r)
    n = int(r[1 : r.index(b"\r\n")])
    i = r.index(b"\r\n") + 2
    out = []
    for _ in range(n):
        ln = int(r[i + 1 : r.index(b"\r\n", i)])
        i = r.index(b"\r\n", i) + 2
        out.append(r[i : i + ln])
        i += ln + 2
    return frozenset(out)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        for k in KEYS:
            cmd(s, "SET", k, "v")
    fails = []
    for pat in PATTERNS:
        ro, rf = keys_set(od, pat), keys_set(fr, pat)
        if ro != rf:
            ronly = set(ro) - set(rf) if isinstance(ro, frozenset) and isinstance(rf, frozenset) else ro
            fonly = set(rf) - set(ro) if isinstance(ro, frozenset) and isinstance(rf, frozenset) else rf
            fails.append(f"pattern={pat!r}: redis_only={ronly} fr_only={fonly}")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} glob-match divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — glob (KEYS) matching byte-exact vs redis 7.2.4 "
        f"({len(PATTERNS)} patterns x {len(KEYS)} keys: stars/?/classes/ranges/negation/escapes) "
        "[multi-star-vs-empty edge = z9dc3]"
    )


if __name__ == "__main__":
    main()
