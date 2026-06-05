#!/usr/bin/env python3
"""resp_protocol_probe.py — raw-socket RESP protocol differential gate.

Byte-compares fr-server against the vendored redis 7.2.4 oracle at the *wire*
level — inline commands, multibulk/bulk framing, and malformed-frame handling
that redis-cli cannot express. This is the regression gate for the command
parser (e.g. the borrowed-argv refactor): run it after any change under
crates/fr-protocol or the fr-server read path.

SETUP (oracle config-less so compiled defaults align; fr in strict mode):
    ORACLE=legacy_redis_code/redis/src
    $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
    # build fr locally; CARGO_TARGET_DIR is /data/tmp/cargo-target here:
    cargo build -p fr-server
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/resp_protocol_probe.py 16399 16400

KNOWN divergence (reported, not a hard fail — bead frankenredis-v4cl4):
  - A multibulk bulk arg whose declared length mismatches its content: redis
    blindly advances `qb_pos += bulklen+2` (no terminator check) and re-splits
    the bytes into the next command; fr validates the trailing CRLF and returns
    a protocol error. fr is strictly MORE validating. Tracked for the parser
    refactor; normalised out here.
"""
import socket
import sys


def probe(port: int, raw: bytes, timeout: float = 0.4) -> bytes:
    s = socket.socket()
    s.settimeout(timeout)
    data = b""
    try:
        s.connect(("127.0.0.1", port))
        s.sendall(raw)
        try:
            while True:
                chunk = s.recv(4096)
                if not chunk:
                    break
                data += chunk
                s.settimeout(0.12)  # drain a little more, then stop
        except socket.timeout:
            pass
    except Exception as exc:  # noqa: BLE001 - report connection faults inline
        data = ("EXC:" + str(exc)).encode()
    finally:
        s.close()
    return data


# (label, raw bytes). Each must elicit a deterministic reply on both servers.
CASES = [
    # ── inline command parsing ───────────────────────────────────────────
    ("inline ping", b"PING\r\n"),
    ("inline set+get", b"SET ik inlineval\r\nGET ik\r\n"),
    ("inline double-quoted", b'SET ik "a b c"\r\nGET ik\r\n'),
    ("inline single-quoted", b"SET ik 'x y'\r\nGET ik\r\n"),
    ("inline unbalanced quote", b'SET ik "unbalanced\r\n'),
    ("inline hex escape", b'SET ik "\\x41\\x42"\r\nGET ik\r\n'),
    ("inline trailing spaces", b"PING   \r\n"),
    ("inline empty line", b"\r\n"),
    ("inline blanks only", b"   \r\n"),
    ("inline collapse spaces", b"GET  ik\r\n"),
    # ── multibulk / bulk header validation ───────────────────────────────
    ("multibulk -1", b"*-1\r\n"),
    ("multibulk 0", b"*0\r\n"),
    ("multibulk overflow count", b"*100000000000\r\n"),
    ("bulk -1 len", b"*1\r\n$-1\r\n"),
    ("bulk overflow len", b"*1\r\n$100000000000\r\n"),
    ("non-bulk element +", b"*1\r\n+PING\r\n"),
    ("non-bulk element :", b"*1\r\n:5\r\n"),
    ("multibulk bad count char", b"*x\r\n"),
    ("bulk bad len char", b"*1\r\n$x\r\n"),
    ("bulk incomplete (no data)", b"*1\r\n$4\r\nPING"),
    ("null byte in arg", b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\n\x00\x01\r\n"),
    ("well-formed pipelined", b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n"),
]

# Inputs hitting the known v4cl4 leniency gap (bulklen != content length).
KNOWN_V4CL4 = [
    ("bulk len<content $3 PING", b"*1\r\n$3\r\nPING\r\n"),
    ("bulk len<content $2 PING", b"*1\r\n$2\r\nPING\r\n"),
    ("two-elem len mismatch", b"*2\r\n$3\r\nGET\r\n$2\r\nkey\r\n"),
    ("wrong terminator bytes", b"*2\r\n$3\r\nGETXX$1\r\nk\r\n"),
]


def main() -> int:
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    fails = 0
    known = 0

    for label, raw in CASES:
        o = probe(op, raw).decode("latin1")
        f = probe(fp, raw).decode("latin1")
        if o != f:
            print(f"DIVERGE [{label}]")
            print(f"    oracle: {o!r}")
            print(f"    fr    : {f!r}")
            fails += 1

    for label, raw in KNOWN_V4CL4:
        o = probe(op, raw).decode("latin1")
        f = probe(fp, raw).decode("latin1")
        if o != f:
            known += 1  # expected divergence; see bead v4cl4
        else:
            # fr started matching redis — the bead is fixed; promote to a real check.
            print(f"NOTE [{label}] now matches oracle — v4cl4 likely fixed, tighten this gate")

    print("------------------------------------------------------------")
    print(f"hard divergences: {fails} | known (v4cl4): {known}")
    if fails == 0:
        print("PASS — fr matches redis 7.2.4 across the probed RESP protocol surface")
        return 0
    print(f"FAIL — {fails} protocol divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
