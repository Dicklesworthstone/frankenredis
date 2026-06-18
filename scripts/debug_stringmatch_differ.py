#!/usr/bin/env python3
"""Differential gate: DEBUG STRINGMATCH-LEN + no-op DEBUG subcommands (frankenredis-glf5v).

DEBUG STRINGMATCH-LEN <pattern> <string> drives redis's stringmatchlen directly — a
DISTINCT path from KEYS/SCAN MATCH that also exercises fr's glob_match, so it
double-checks the z9dc3 fix (`*`/`**` vs the empty string) from another angle. This
gate pins it plus a few connection-independent no-op DEBUG subcommands (JMAP,
SET-ACTIVE-EXPIRE 0/1, QUICKLIST-PACKED-THRESHOLD) and the unknown-subcommand error,
byte-exact vs redis 7.2.4. (DEBUG OBJECT is intentionally excluded — its at:/lru:
fields are a documented architectural divergence.)

Usage: debug_stringmatch_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.02)
    return s.recv(1 << 20)


CASES = [
    # DEBUG STRINGMATCH-LEN — glob via the stringmatchlen path
    ["DEBUG", "STRINGMATCH-LEN", "h?llo", "hello"],
    ["DEBUG", "STRINGMATCH-LEN", "h?llo", "hallo"],
    ["DEBUG", "STRINGMATCH-LEN", "h?llo", "hllo"],
    ["DEBUG", "STRINGMATCH-LEN", "*", "anything"],
    ["DEBUG", "STRINGMATCH-LEN", "*", ""],          # z9dc3 area
    ["DEBUG", "STRINGMATCH-LEN", "**", ""],         # z9dc3: ** vs empty
    ["DEBUG", "STRINGMATCH-LEN", "***", ""],
    ["DEBUG", "STRINGMATCH-LEN", "", ""],
    ["DEBUG", "STRINGMATCH-LEN", "[a-z]*", "hello"],
    ["DEBUG", "STRINGMATCH-LEN", "[^a]bc", "xbc"],
    ["DEBUG", "STRINGMATCH-LEN", "[^a]bc", "abc"],
    ["DEBUG", "STRINGMATCH-LEN", "abc", "xyz"],
    ["DEBUG", "STRINGMATCH-LEN", "h\\*llo", "h*llo"],
    ["DEBUG", "STRINGMATCH-LEN", "h[ae]llo", "hallo"],
    ["DEBUG", "STRINGMATCH-LEN", "a*b*c", "axxbxxc"],
    # no-op / setter DEBUG subcommands (connection-independent +OK)
    ["DEBUG", "JMAP"],
    ["DEBUG", "SET-ACTIVE-EXPIRE", "1"],
    ["DEBUG", "SET-ACTIVE-EXPIRE", "0"],
    ["DEBUG", "QUICKLIST-PACKED-THRESHOLD", "100"],
    ["DEBUG", "QUICKLIST-PACKED-THRESHOLD", "1K"],
    ["DEBUG", "QUICKLIST-PACKED-THRESHOLD", "0"],
    ["debug", "jmap"],                              # case-insensitive
    ["DEBUG", "NOSUCHSUBCMD"],                       # unknown-subcommand error
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    cmd(od, "DEBUG", "SET-ACTIVE-EXPIRE", "1")
    cmd(fr, "DEBUG", "SET-ACTIVE-EXPIRE", "1")
    fails = []
    for argv in CASES:
        ro, rf = cmd(od, *argv), cmd(fr, *argv)
        if ro != rf:
            fails.append(f"{' '.join(argv)!r}: redis={ro!r} fr={rf!r}")
    # leave active-expire enabled
    cmd(od, "DEBUG", "SET-ACTIVE-EXPIRE", "1")
    cmd(fr, "DEBUG", "SET-ACTIVE-EXPIRE", "1")
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} DEBUG subcommand divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — DEBUG STRINGMATCH-LEN + no-op subcommands byte-exact vs redis 7.2.4 "
        f"({len(CASES)} cases: glob-via-DEBUG incl z9dc3 */** vs empty)"
    )


if __name__ == "__main__":
    main()
