#!/usr/bin/env python3
"""strict_limit_gate.py — raw-socket gate for the STRICT-mode protocol/value
limit boundary, byte-compared against the vendored redis 7.2.4 oracle.

This is the regression gate for frankenredis-ktdsz (and its class): fr's
`CompatibilityGate` hardened safety caps (max_array_len=1024, max_bulk_len=8MiB)
must NOT apply in strict mode — strict mode maximizes observable compatibility,
so the only limits are redis's own: multibulk count up to INT_MAX, a single
bulk up to proto-max-bulk-len (512MiB), and the string-growth ceiling enforced
by checkStringLength. The hardened-mode caps live in `CompatibilityGate::hardened()`.

The fr-config unit test asserts the gate *values*; this gate proves the running
server's WIRE behavior — i.e. that those values flow through `parser_config`
(`max_bulk_len.min(proto_max_bulk_len)`) and the command layer correctly. A
parser/IO refactor (e.g. zero-copy RESP) could silently reintroduce a strict
cap or break proto-max-bulk-len enforcement; run this after any change under
crates/fr-protocol, crates/fr-config, or the fr-server read path.

SETUP (oracle config-less so compiled defaults align; fr in strict mode):
    ORACLE=legacy_redis_code/redis/src
    $ORACLE/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    cargo build -p fr-server   # CARGO_TARGET_DIR is /data/tmp/cargo-target here
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/strict_limit_gate.py 16399 16400
"""
import socket
import sys

PROTO_MAX_BULK = 512 * 1024 * 1024  # redis default proto-max-bulk-len
OLD_ARRAY_CAP = 1024  # the (now hardened-only) CompatibilityGate cap
OLD_BULK_CAP = 8 * 1024 * 1024  # the (now hardened-only) CompatibilityGate cap


def probe(port: int, raw: bytes, timeout: float = 1.0) -> bytes:
    s = socket.socket()
    s.settimeout(timeout)
    data = b""
    try:
        s.connect(("127.0.0.1", port))
        s.sendall(raw)
        try:
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                data += chunk
                s.settimeout(0.15)  # drain a little more, then stop
        except socket.timeout:
            pass
    except Exception as exc:  # noqa: BLE001 - report connection faults inline
        data = ("EXC:" + str(exc)).encode()
    finally:
        s.close()
    return data


def multibulk(*args: bytes) -> bytes:
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def del_of(n: int) -> bytes:
    # DEL of n nonexistent keys -> :0 on both. Proves >OLD_ARRAY_CAP args accepted.
    return multibulk(b"DEL", *[b"nk%d" % i for i in range(n)])


def set_value(nbytes: int) -> bytes:
    return multibulk(b"SET", b"k", b"v" * nbytes)


# (label, raw bytes). Each must elicit a byte-identical reply on both servers.
CASES = [
    # ── proto-max-bulk-len enforcement still fires (strict gate is usize::MAX,
    #    but parser_config .min(proto_max_bulk_len) keeps the 512MiB ceiling) ──
    ("bulk len = 512MiB+1 (over proto-max-bulk-len)",
     multibulk(b"SET", b"k", b"x") + b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$%d\r\n" % (PROTO_MAX_BULK + 1)),
    ("bulk len = 1GiB (over)", b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$%d\r\n" % (1024 * 1024 * 1024)),
    ("bulk len negative", b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$-1\r\n"),
    ("bulk len non-numeric", b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$notanum\r\n"),
    ("multibulk count huge (over INT_MAX)", b"*99999999999999\r\n"),

    # ── ktdsz core: large multibulk ACCEPTED in strict mode (was capped @1024) ──
    ("multibulk 1024 args (old cap)", del_of(OLD_ARRAY_CAP)),
    ("multibulk 1025 args (just over old cap)", del_of(OLD_ARRAY_CAP + 1)),
    ("multibulk 5000 args", del_of(5000)),

    # ── ktdsz core: large bulk ACCEPTED in strict mode (was capped @8MiB) ──
    ("bulk 8MiB (old cap, exact)", set_value(OLD_BULK_CAP)),
    ("bulk 8MiB+1 (just over old cap)", set_value(OLD_BULK_CAP + 1)),
    ("bulk 16MiB", set_value(16 * 1024 * 1024)),

    # ── checkStringLength growth ceiling (command layer, == proto-max-bulk-len) ──
    ("SETRANGE final len = 512MiB (at limit, ok)",
     multibulk(b"DEL", b"k") + multibulk(b"SETRANGE", b"k", b"%d" % (PROTO_MAX_BULK - 1), b"x")),
    ("SETRANGE final len = 512MiB+1 (over -> error)",
     multibulk(b"DEL", b"k") + multibulk(b"SETRANGE", b"k", b"%d" % PROTO_MAX_BULK, b"x")),
    ("SETBIT bit = 512MiB*8-1 (last valid bit, ok)",
     multibulk(b"DEL", b"k") + multibulk(b"SETBIT", b"k", b"%d" % (PROTO_MAX_BULK * 8 - 1), b"1")),
    ("SETBIT bit = 512MiB*8 (over -> out of range)",
     multibulk(b"DEL", b"k") + multibulk(b"SETBIT", b"k", b"%d" % (PROTO_MAX_BULK * 8), b"1")),
    ("SETBIT bit negative", multibulk(b"SETBIT", b"k", b"-1", b"1")),
    ("BITFIELD SET huge bit offset",
     multibulk(b"BITFIELD", b"k", b"SET", b"u8", b"#999999999", b"255")),
]


def main() -> int:
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    fails = 0
    for label, raw in CASES:
        # FLUSH between cases so 512MiB strings don't pile up.
        probe(op, b"*1\r\n$8\r\nFLUSHALL\r\n")
        probe(fp, b"*1\r\n$8\r\nFLUSHALL\r\n")
        o = probe(op, raw).decode("latin1")
        f = probe(fp, raw).decode("latin1")
        status = "ok" if o == f else "DIVERGE"
        print(f"[{status}] {label}")
        if o != f:
            print(f"    oracle: {o!r}")
            print(f"    fr    : {f!r}")
            fails += 1
    print("------------------------------------------------------------")
    if fails == 0:
        print("PASS — strict-mode limit boundary matches redis 7.2.4 (frankenredis-ktdsz held)")
        return 0
    print(f"FAIL — {fails} strict-limit divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
