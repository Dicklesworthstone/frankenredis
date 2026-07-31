#!/usr/bin/env python3
"""Prove the sharded bus never strands a reply and never reorders one.

WHY THIS EXISTS, AND WHY IT IS A RAW SOCKET
-------------------------------------------
Per-tick shard regrouping (frankenredis-odusj) makes a command's journey span a
staging buffer that is flushed at the END of an event-loop tick. The failure mode
that introduces is not a wrong answer, it is SILENCE: a job left in staging when
the loop returns to poll is a reply that never arrives. So the gate has to be a
liveness gate with a hard timeout, not a correctness spot-check.

An earlier attempt at this used `redis-cli --pipe` and reported failure on BOTH
binaries, which looked like a real regression and was worthless: --pipe terminates
its stream with an ECHO sentinel, and sharded mode refuses ECHO, so that check
could never have passed on any binary. This drives a raw socket and counts RESP
replies itself, so nothing outside the code under test can decide the verdict.

WHAT IT CHECKS
--------------
1. LIVENESS -- every request gets a reply, under a hard wall-clock timeout. The
   whole pipeline is written BEFORE anything is read, so the server must handle
   a full read buffer of scattered keys and flush staging on its own schedule
   rather than being driven one command at a time.
2. ORDER -- replies come back in request order. Keys are chosen to scatter across
   shards, so completions genuinely arrive out of order inside the server, and
   ShardedReplyOrder is what puts them back. A regrouping bug that mixed up
   sequences would surface here.
3. VALUES -- each GET returns the value its preceding SET wrote for that key, so
   per-key FIFO survived being split across staging flushes.
4. SHARD SPREAD -- asserts the fixture actually spans multiple shards at this
   worker count, so a passing run cannot be an accident of every key landing on
   one worker.
"""

import argparse
import socket
import sys
import time

POLY = 0x1021
_TAB = []
for _i in range(256):
    _crc = _i << 8
    for _ in range(8):
        _crc = ((_crc << 1) ^ POLY) & 0xFFFF if _crc & 0x8000 else (_crc << 1) & 0xFFFF
    _TAB.append(_crc)


def crc16_slot(key: bytes) -> int:
    """CRC-16/XMODEM %16384, matching fr_store::crc16_slot for untagged keys."""
    crc = 0
    for b in key:
        crc = ((crc << 8) & 0xFF00) ^ _TAB[((crc >> 8) ^ b) & 0xFF]
    return crc % 16384


for _probe, _expected in ((b"foo", 12182), (b"bar", 5061), (b"hello", 866)):
    if crc16_slot(_probe) != _expected:
        raise SystemExit(f"CRC16 self-test failed on {_probe!r}")


def encode(*parts: bytes) -> bytes:
    out = [b"*%d\r\n" % len(parts)]
    for p in parts:
        out.append(b"$%d\r\n%s\r\n" % (len(p), p))
    return b"".join(out)


class ReplyReader:
    """Incremental RESP reader that counts complete top-level replies."""

    def __init__(self):
        self.buf = b""

    def feed(self, data: bytes):
        self.buf += data

    def take(self):
        """Yield every complete reply currently buffered."""
        while True:
            reply, rest = self._parse(self.buf)
            if reply is None:
                return
            self.buf = rest
            yield reply

    def _parse(self, buf: bytes):
        if not buf:
            return None, buf
        kind = buf[:1]
        nl = buf.find(b"\r\n")
        if nl < 0:
            return None, buf
        head, rest = buf[:nl], buf[nl + 2 :]
        if kind in (b"+", b"-", b":"):
            return head, rest
        if kind == b"$":
            n = int(head[1:])
            if n == -1:
                return b"$-1", rest
            if len(rest) < n + 2:
                return None, buf
            return rest[:n], rest[n + 2 :]
        raise SystemExit(f"unexpected RESP reply type {kind!r} in {buf[:64]!r}")


def run(host, port, count, workers, timeout_s, value_size):
    keys = [f"ordk:{i}".encode() for i in range(count)]
    shards = {crc16_slot(k) % workers for k in keys}
    if workers > 1 and len(shards) < 2:
        raise SystemExit(
            f"FIXTURE INVALID: all {count} keys map to {len(shards)} shard(s) at W={workers}"
        )

    value = b"v" * value_size
    requests = []
    expected = []
    for k in keys:
        requests.append(encode(b"SET", k, value))
        expected.append(("ok", b"+OK"))
        requests.append(encode(b"GET", k))
        expected.append(("val", value))
    payload = b"".join(requests)

    sock = socket.create_connection((host, port), timeout=timeout_s)
    sock.settimeout(timeout_s)
    deadline = time.monotonic() + timeout_s
    # Write the WHOLE pipeline before reading a single byte: the server must
    # make progress on its own, which is exactly what a stranded staging buffer
    # would prevent.
    sock.sendall(payload)

    reader = ReplyReader()
    got = []
    while len(got) < len(expected):
        if time.monotonic() > deadline:
            sock.close()
            return (
                False,
                f"TIMEOUT after {timeout_s}s with {len(got)}/{len(expected)} replies "
                f"-- a stranded staging buffer looks exactly like this",
                len(shards),
            )
        try:
            chunk = sock.recv(1 << 20)
        except socket.timeout:
            sock.close()
            return (
                False,
                f"RECV TIMEOUT with {len(got)}/{len(expected)} replies",
                len(shards),
            )
        if not chunk:
            sock.close()
            return (
                False,
                f"SERVER CLOSED with {len(got)}/{len(expected)} replies",
                len(shards),
            )
        reader.feed(chunk)
        got.extend(reader.take())
    sock.close()

    if len(got) != len(expected):
        return False, f"reply COUNT {len(got)} != {len(expected)}", len(shards)
    for i, ((kind, want), have) in enumerate(zip(expected, got)):
        if have != want:
            return (
                False,
                f"reply {i} ({kind}) OUT OF ORDER or WRONG: {have[:40]!r} != {want[:40]!r}",
                len(shards),
            )
    return True, f"{len(got)}/{len(expected)} replies exact and in order", len(shards)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--workers", type=int, required=True,
                    help="worker count the server was started with (fixture spread check)")
    ap.add_argument("--count", type=int, default=2000, help="key count; 2 commands each")
    ap.add_argument("--timeout", type=float, default=20.0)
    ap.add_argument("--value-size", type=int, default=16)
    args = ap.parse_args()

    ok, detail, spread = run(
        args.host, args.port, args.count, args.workers, args.timeout, args.value_size
    )
    status = "PASS" if ok else "FAIL"
    print(f"{status} W={args.workers} keys={args.count} shards_touched={spread}: {detail}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
