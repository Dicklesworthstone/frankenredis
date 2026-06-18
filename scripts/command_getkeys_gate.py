#!/usr/bin/env python3
"""command_getkeys_gate.py — COMMAND GETKEYS / GETKEYSANDFLAGS differential gate
vs the vendored redis 7.2.4 oracle.

Locks in the movable-key / per-keyspec metadata surface (the area
frankenredis-agx04 fixed): the generic write fallback used to mis-flag movable
and per-keyspec commands. There is no other gate for this surface, and it is
directly threatened by hot-path dispatch refactors (borrowed fast-paths,
flag/keyspec representation changes), so re-run it after any change under
crates/fr-command's COMMAND_TABLE / keyspec / getkeys logic.

SETUP (oracle config-less => compiled defaults; fr in strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    cargo build -p fr-server   # CARGO_TARGET_DIR is /data/tmp/cargo-target here
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/command_getkeys_gate.py 16399 16400
"""
import socket
import sys


def cmd(port: int, *args) -> bytes:
    s = socket.create_connection(("127.0.0.1", port))
    s.settimeout(3)
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    data = b""
    s.settimeout(0.5)
    try:
        while True:
            ch = s.recv(4096)
            if not ch:
                break
            data += ch
    except socket.timeout:
        pass
    s.close()
    return data


# Tricky movable-key / numkeys / keyspec commands.
CASES = [
    ("GET", "k"), ("SET", "k", "v"), ("MSET", "k1", "v1", "k2", "v2"),
    ("MGET", "k1", "k2", "k3"), ("DEL", "k1", "k2"),
    ("EXISTS", "k1", "k2"), ("UNLINK", "k1", "k2"),
    ("GETRANGE", "k", "0", "-1"), ("SETRANGE", "k", "0", "x"),
    ("ZADD", "z", "1", "a"), ("ZADD", "z", "GT", "CH", "1", "a"),
    ("GEORADIUS", "g", "0", "0", "1", "m", "STORE", "dest"),
    ("GEORADIUS", "g", "0", "0", "1", "m", "STOREDIST", "dest", "COUNT", "5"),
    ("GEORADIUS", "g", "0", "0", "1", "m"),
    ("GEOSEARCHSTORE", "dst", "src", "FROMMEMBER", "m", "BYRADIUS", "1", "m", "ASC"),
    ("SORT", "mylist", "STORE", "dest"),
    ("SORT", "mylist", "BY", "w_*", "GET", "o_*", "STORE", "dest"),
    ("SORT", "mylist"), ("SORT_RO", "mylist"),
    ("EVAL", "return 1", "2", "k1", "k2", "arg1"),
    ("EVALSHA", "abc", "1", "k1"), ("FCALL", "f", "2", "k1", "k2", "a"),
    ("ZUNIONSTORE", "dest", "2", "z1", "z2"),
    ("ZINTERSTORE", "dest", "2", "z1", "z2", "WEIGHTS", "1", "2"),
    ("ZUNION", "2", "z1", "z2"), ("ZDIFF", "2", "z1", "z2"),
    ("ZINTERCARD", "2", "z1", "z2", "LIMIT", "3"),
    ("ZRANGESTORE", "d", "s", "0", "-1"),
    ("XREAD", "COUNT", "2", "STREAMS", "s1", "s2", "0", "0"),
    ("XREADGROUP", "GROUP", "g", "c", "STREAMS", "s1", "0"),
    ("LMPOP", "2", "l1", "l2", "LEFT"), ("ZMPOP", "2", "z1", "z2", "MIN"),
    ("SINTERCARD", "2", "s1", "s2", "LIMIT", "1"),
    ("BLMPOP", "0", "2", "l1", "l2", "LEFT"), ("BZMPOP", "0", "2", "z1", "z2", "MIN"),
    ("BLPOP", "l1", "l2", "0"), ("BRPOP", "l1", "l2", "0"),
    ("BRPOPLPUSH", "src", "dst", "0"), ("BLMOVE", "src", "dst", "LEFT", "RIGHT", "0"),
    ("BZPOPMIN", "z1", "z2", "0"), ("BZPOPMAX", "z1", "z2", "0"),
    ("LMOVE", "src", "dst", "LEFT", "RIGHT"), ("RPOPLPUSH", "src", "dst"),
    ("SMOVE", "src", "dst", "m"),
    ("COPY", "src", "dst"), ("COPY", "src", "dst", "REPLACE"),
    ("COPY", "src", "dst", "DB", "1"), ("COPY", "src", "dst", "DB", "1", "REPLACE"),
    ("MOVE", "src", "1"),
    ("RENAME", "src", "dst"), ("RENAMENX", "src", "dst"),
    ("PFCOUNT", "h1", "h2"), ("PFADD", "h", "a"), ("PFMERGE", "dst", "h1", "h2"),
    ("MIGRATE", "host", "6379", "", "0", "1000", "KEYS", "k1", "k2"),
    ("GETEX", "k", "EX", "10"), ("GETDEL", "k"), ("APPEND", "k", "v"),
    ("TTL", "k"), ("PTTL", "k"), ("PERSIST", "k"), ("TOUCH", "k1", "k2"), ("TYPE", "k"),
    ("SINTERSTORE", "d", "s1", "s2"), ("SINTER", "s1", "s2"),
    ("SDIFFSTORE", "d", "s1", "s2"),
    ("HSET", "h", "f", "v"), ("HRANDFIELD", "h", "2"),
    ("BITOP", "AND", "dest", "s1", "s2"), ("BITCOUNT", "k", "0", "-1"),
    ("BITFIELD", "k", "GET", "u8", "0"), ("BITFIELD_RO", "k", "GET", "u8", "0"),
    ("DUMP", "k"), ("RESTORE", "k", "0", "payload"),
    ("EXPIRE", "k", "10"), ("OBJECT", "ENCODING", "k"), ("MEMORY", "USAGE", "k"),
    ("WATCH", "k1", "k2"), ("XADD", "s", "*", "f", "v"),
    ("GEOADD", "g", "0", "0", "m"), ("WAIT", "0", "100"),
]

SUBS = ["GETKEYS", "GETKEYSANDFLAGS"]


def main() -> int:
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    fails = 0
    total = 0
    for sub in SUBS:
        for c in CASES:
            total += 1
            o = cmd(op, "COMMAND", sub, *c)
            f = cmd(fp, "COMMAND", sub, *c)
            if o != f:
                fails += 1
                print(f"DIVERGE COMMAND {sub} {c}")
                print(f"    oracle: {o!r}")
                print(f"    fr    : {f!r}")
    print("------------------------------------------------------------")
    print(f"checked {total} (COMMAND GETKEYS + GETKEYSANDFLAGS); divergences: {fails}")
    if fails == 0:
        print("PASS — fr COMMAND GETKEYS/GETKEYSANDFLAGS matches redis 7.2.4")
        return 0
    print(f"FAIL — {fails} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
