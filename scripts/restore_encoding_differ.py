#!/usr/bin/env python3
"""Differential gate: OBJECT ENCODING after a RESTORE command matches redis 7.2.4.

dump_restore_fuzz.py proves the DUMP *payload bytes* round-trip identically, but
it does not assert that the value RESTORE rebuilds lands in the SAME OBJECT
ENCODING upstream would pick. RESTORE re-decodes a serialized payload and re-runs
the encoding-selection logic, so a wrong listpack/intset/quicklist/hashtable
decision there is invisible to a byte-compat check yet visible to clients and
monitoring (cf. the encoding-after-reload class: hpfey). This gate builds values
spanning every type and both sides of the small/large encoding boundary, DUMPs +
RESTOREs each on each server, and asserts:
  (a) the original key's OBJECT ENCODING matches between fr and redis, and
  (b) the RESTORE'd key's OBJECT ENCODING matches between fr and redis.

Both servers run config-LESS (compiled defaults) so encodings align without the
config-default false-positive class.

SETUP:
  ORACLE=legacy_redis_code/redis/src
  $ORACLE/redis-server --port 17831 --daemonize yes --save '' --appendonly no
  $CARGO_TARGET_DIR/debug/frankenredis --port 17832 --mode strict &
  scripts/restore_encoding_differ.py --oracle 17831 --fr 17832

Exit status: 0 = byte-exact, 1 = at least one divergence (details printed).
"""
import argparse

from _respread import assert_ok, assert_seed, cmd, conn


def build(c):
    """Populate one value per (type x encoding-side-of-boundary)."""
    assert_ok(cmd(c, "FLUSHALL"), "FLUSHALL")
    assert_seed(cmd(c, "RPUSH", "Lsmall", "a", "b", "c"), 3, "RPUSH Lsmall")
    assert_seed(cmd(c, "RPUSH", "Lbig", *[f"elem{i:05}" for i in range(200)]), 200, "RPUSH Lbig")
    assert_seed(cmd(c, "RPUSH", "Lbigval", "x" * 128), 1, "RPUSH Lbigval")
    assert_seed(cmd(c, "SADD", "Sint", "1", "2", "3"), 3, "SADD Sint")
    assert_seed(cmd(c, "SADD", "Sintbig", *[str(i) for i in range(600)]), 600, "SADD Sintbig")
    assert_seed(cmd(c, "SADD", "Slp", "a", "b", "c"), 3, "SADD Slp")
    assert_seed(cmd(c, "SADD", "Sbig", *[f"m{i}" for i in range(200)]), 200, "SADD Sbig")
    assert_seed(cmd(c, "HSET", "Hsmall", "a", "1", "b", "2"), 2, "HSET Hsmall")
    for i in range(200):
        assert_seed(cmd(c, "HSET", "Hbig", f"f{i}", f"v{i}"), 1, f"HSET Hbig f{i}")
    assert_seed(cmd(c, "ZADD", "Zsmall", "1", "a", "2", "b"), 2, "ZADD Zsmall")
    for i in range(200):
        assert_seed(cmd(c, "ZADD", "Zbig", i, f"m{i}"), 1, f"ZADD Zbig m{i}")
    assert_ok(cmd(c, "SET", "Sint64", "12345"), "SET Sint64")
    assert_ok(cmd(c, "SET", "Sembstr", "hello"), "SET Sembstr")
    assert_ok(cmd(c, "SET", "Sraw", "x" * 64), "SET Sraw")
    assert_seed(cmd(c, "DBSIZE"), len(KEYS), "restore-encoding fixture key count")


KEYS = ["Lsmall", "Lbig", "Lbigval", "Sint", "Sintbig", "Slp", "Sbig",
        "Hsmall", "Hbig", "Zsmall", "Zbig", "Sint64", "Sembstr", "Sraw"]


def dump_payload(reply, label):
    """Extract a non-null bulk payload without masking a failed DUMP seed."""
    if not reply.startswith(b"$"):
        raise SystemExit(f"SEED FAILED [{label} DUMP]: expected bulk payload, got {reply!r}")
    header_end = reply.find(b"\r\n")
    try:
        length = int(reply[1:header_end])
    except ValueError as exc:
        raise SystemExit(f"SEED FAILED [{label} DUMP]: malformed bulk reply {reply!r}") from exc
    payload = reply[header_end + 2:-2]
    if length < 0 or len(payload) != length:
        raise SystemExit(
            f"SEED FAILED [{label} DUMP]: expected {length} payload bytes, got {len(payload)}"
        )
    return payload


def record_diff(label, oracle, fr):
    """Return one precisely when this gate's byte-level comparison detects a mismatch."""
    if oracle == fr:
        return 0
    print(f"{label}: oracle={oracle!r} fr={fr!r}")
    return 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument(
        "--planted-negative",
        action="store_true",
        help="prove the same comparison path rejects a deliberately wrong encoding reply",
    )
    args = ap.parse_args()
    if args.planted_negative:
        return record_diff("PLANTED NEGATIVE detected", b"$8\r\nlistpack\r\n", b"$9\r\nhashtable\r\n")

    o = conn(args.oracle)
    f = conn(args.fr)
    build(o)
    build(f)

    diffs = 0
    try:
        for k in KEYS:
            # Original-encoding parity.
            oo, of = cmd(o, "OBJECT", "ENCODING", k), cmd(f, "OBJECT", "ENCODING", k)
            diffs += record_diff(f"ORIG-ENC DIVERGE {k}", oo, of)
            # DUMP + RESTORE on each server, then compare the RESTORE'd encoding.
            for engine, c in (("redis", o), ("fr", f)):
                payload = dump_payload(cmd(c, "DUMP", k), f"{engine} {k}")
                assert_seed(cmd(c, "DEL", k + "_r"), 0, f"{engine} DEL {k}_r")
                assert_ok(cmd(c, "RESTORE", k + "_r", "0", payload), f"{engine} RESTORE {k}")
            eo, ef = cmd(o, "OBJECT", "ENCODING", k + "_r"), cmd(f, "OBJECT", "ENCODING", k + "_r")
            diffs += record_diff(f"RESTORE-ENC DIVERGE {k}", eo, ef)
    finally:
        for c in (o, f):
            try:
                cmd(c, "FLUSHALL")
            except Exception:
                pass
            c.close()

    if diffs:
        print(f"\nFAIL: {diffs} encoding divergence(s) (original and/or post-RESTORE)")
        return 1
    print("OK: OBJECT ENCODING byte-exact for original AND post-RESTORE values "
          "vs redis 7.2.4 (list/set/intset/hash/zset/string across encoding boundaries)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
