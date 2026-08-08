#!/usr/bin/env python3
"""hash_restore_read_premise_probe.py — does fr actually LOSE RESTORE+read on small hashes?

WHY THIS EXISTS (frankenredis-b1o02 preflight)
----------------------------------------------
b1o02 proposes storing the raw RDB listpack verbatim as the in-memory small-hash
repr (zero decode on RESTORE, zero encode on DUMP) with O(n) listpack scans for
reads -- i.e. redis's shallow attach.

docs/NEGATIVE_EVIDENCE.md:5478 (2026-07-23 FoggyOrchid) already REJECTED that
mechanism for LISTS as a measurement artifact: RESTORE-in-isolation showed fr
retiring 2.749x more instructions, yet RESTORE+LRANGE had fr 11% FASTER end to
end, because eager decode buys O(1) indexed reads while the shallow attach pays
an O(n) listpack walk on EVERY read. Its closing instruction is general:
"Do not re-file the RESTORE-isolation gap as a loss; measure RESTORE+read."

b1o02's premise ("HASH 0.57x") is a RESTORE-ISOLATION number, so it is under
exactly that instruction. But the documented hash read wins (HGETALL 3.81x etc.)
are on a 500-field NUMERIC hash, which is hashtable-encoded and would NOT take
the proposed variant. The lever's real blast radius is SMALL listpack hashes,
where fr's read advantage is UNMEASURED. This probe measures it.

DECISION RULE (pre-registered, stated before running):
  * If fr already wins (or ties) RESTORE+HGETALL on small listpack hashes, the
    lever trades a read win for a restore win and b1o02 closes as premise-reject.
  * If fr loses RESTORE+HGETALL, the lever is justified and b1o02 proceeds.
  * RESTORE-only is reported for continuity with the ledger but DOES NOT decide.

METHOD, and the two ways this was gotten wrong before (both recorded on b1o02)
-----------------------------------------------------------------------------
  1. FIXED TIME is unsound. `perf stat -p PID -- sleep 6` charges each engine for
     whatever it managed in 6s: that measures throughput, not per-op cost. This
     probe runs a FIXED NUMBER OF OPERATIONS and stops perf when the last
     expected reply byte has been drained.
  2. sendall() of the whole batch DEADLOCKS: replies fill the socket buffer while
     the client is still writing. This probe runs the writer on its own thread and
     drains concurrently in the reader, so neither direction can block the other.

instructions:u is user-space only and deliberately measures compute, not the
kernel path; wall ns/op is reported alongside it, not instead of it.
"""

from __future__ import annotations

import argparse
import signal
import socket
import subprocess
import sys
import threading
import time
from statistics import median

OK_REPLY_LEN = len(b"+OK\r\n")


def resp(*parts: bytes) -> bytes:
    out = b"*%d\r\n" % len(parts)
    for part in parts:
        out += b"$%d\r\n%s\r\n" % (len(part), part)
    return out


class Client:
    def __init__(self, port: int) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""

    def command(self, *parts: bytes) -> bytes:
        """Send one command and return its complete raw reply."""
        self.sock.sendall(resp(*parts))
        return self._read_one_reply()

    def _read_one_reply(self) -> bytes:
        # Small helper for setup traffic only -- parses just enough RESP to know
        # when a single reply is complete.
        while True:
            complete, consumed = _reply_complete(self.buf)
            if complete:
                reply, self.buf = self.buf[:consumed], self.buf[consumed:]
                return reply
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("server closed during setup")
            self.buf += chunk

    def close(self) -> None:
        self.sock.close()


def _reply_complete(buf: bytes) -> tuple[bool, int]:
    if not buf:
        return False, 0
    kind = buf[:1]
    if kind in (b"+", b"-", b":"):
        end = buf.find(b"\r\n")
        return (True, end + 2) if end >= 0 else (False, 0)
    if kind == b"$":
        end = buf.find(b"\r\n")
        if end < 0:
            return False, 0
        length = int(buf[1:end])
        if length == -1:
            return True, end + 2
        total = end + 2 + length + 2
        return (True, total) if len(buf) >= total else (False, 0)
    if kind == b"*":
        end = buf.find(b"\r\n")
        if end < 0:
            return False, 0
        count = int(buf[1:end])
        offset = end + 2
        if count == -1:
            return True, offset
        for _ in range(count):
            complete, consumed = _reply_complete(buf[offset:])
            if not complete:
                return False, 0
            offset += consumed
        return True, offset
    raise RuntimeError(f"unparsable reply prefix {kind!r}")


def build_reference_hash(client: Client, key: bytes, fields: int, value_len: int) -> None:
    client.command(b"DEL", key)
    args: list[bytes] = [b"HSET", key]
    for index in range(fields):
        args.append(b"f%04d" % index)
        args.append(b"v" * value_len)
    reply = client.command(*args)
    if not reply.startswith(b":"):
        raise RuntimeError(f"HSET failed: {reply!r}")


def fixed_work_window(port: int, payload: bytes, ops: int, reads: int,
                      hgetall_reply_len: int) -> float:
    """Run exactly `ops` RESTORE (optionally +HGETALL) and return wall seconds.

    The writer runs on its own thread while the caller's thread drains, so the
    server's replies can never wedge the client mid-send.
    """
    sock = socket.create_connection(("127.0.0.1", port), timeout=60)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    request = bytearray()
    for index in range(ops):
        key = b"pk:%d" % index
        request += resp(b"RESTORE", key, b"0", payload)
        for _ in range(reads):
            request += resp(b"HGETALL", key)
    request = bytes(request)

    expected = ops * OK_REPLY_LEN + ops * reads * hgetall_reply_len

    error: list[BaseException] = []

    def write_all() -> None:
        try:
            sock.sendall(request)
        except BaseException as exc:  # surfaced after the join
            error.append(exc)

    writer = threading.Thread(target=write_all, daemon=True)
    started = time.perf_counter()
    try:
        writer.start()
        seen = 0
        first_chunk = b""
        while seen < expected:
            chunk = sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("server closed mid-window")
            if not first_chunk:
                first_chunk = chunk[:64]
            seen += len(chunk)
        elapsed = time.perf_counter() - started
        writer.join(timeout=30)
    finally:
        # A failed rep must not leak its socket into the next one.
        sock.close()
    if error:
        raise error[0]
    if first_chunk.startswith(b"-"):
        raise RuntimeError(f"server returned an error reply: {first_chunk!r}")
    if seen != expected:
        raise RuntimeError(f"drained {seen} bytes, expected exactly {expected}")
    return elapsed


def measure(engine: str, port: int, pid: int, payload: bytes, ops: int,
            reads: int, hgetall_reply_len: int, setup: Client) -> dict:
    setup.command(b"FLUSHALL")
    perf = subprocess.Popen(
        ["perf", "stat", "-e", "instructions:u", "-p", str(pid)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    time.sleep(0.4)  # let perf attach; the server is idle so this costs ~nothing
    if perf.poll() is not None:
        raise RuntimeError(
            "perf exited before the window: " + perf.stderr.read().decode()[:400]
        )
    elapsed = fixed_work_window(port, payload, ops, reads, hgetall_reply_len)
    perf.send_signal(signal.SIGINT)
    perf.wait(timeout=30)
    text = perf.stderr.read().decode()
    instructions = None
    for line in text.splitlines():
        stripped = line.strip()
        if "instructions:u" in stripped:
            instructions = int(stripped.split()[0].replace(",", "").replace(".", ""))
            break
    if instructions is None:
        raise RuntimeError(f"no instructions:u in perf output:\n{text}")
    return {
        "engine": engine,
        "instructions": instructions,
        "instr_per_op": instructions / ops,
        "wall_ns_per_op": elapsed * 1e9 / ops,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fr-port", type=int, required=True)
    parser.add_argument("--fr-pid", type=int, required=True)
    parser.add_argument("--redis-port", type=int, required=True)
    parser.add_argument("--redis-pid", type=int, required=True)
    parser.add_argument("--fields", type=int, default=64,
                        help="fields in the reference hash (<=128 keeps it listpack)")
    parser.add_argument("--value-len", type=int, default=16,
                        help="value length (<=64 keeps it listpack)")
    parser.add_argument("--ops", type=int, default=40000)
    parser.add_argument("--reps", type=int, default=3)
    args = parser.parse_args()

    fr = Client(args.fr_port)
    try:
        redis = Client(args.redis_port)
    except BaseException:
        fr.close()
        raise
    try:
        return _run(args, fr, redis)
    finally:
        fr.close()
        redis.close()


def _run(args, fr: Client, redis: Client) -> int:
    key = b"b1o02:ref"

    payloads = {}
    for name, client in (("fr", fr), ("redis", redis)):
        build_reference_hash(client, key, args.fields, args.value_len)
        encoding = client.command(b"OBJECT", b"ENCODING", key)
        if b"listpack" not in encoding:
            print(f"PRECONDITION FAILED: {name} encodes the reference hash as "
                  f"{encoding!r}, not listpack. This probe only speaks to the "
                  f"small-listpack-hash case b1o02 targets.")
            return 2
        dump = client.command(b"DUMP", key)
        header = dump.split(b"\r\n", 1)[0]
        payloads[name] = dump[len(header) + 2:-2]
        print(f"PRECONDITION {name} object_encoding=listpack dump_payload_bytes={len(payloads[name])}")

    if payloads["fr"] != payloads["redis"]:
        print("NOTE: DUMP payloads differ byte-for-byte between engines; "
              "each engine is fed its own payload so neither is handicapped.")
    else:
        print("PRECONDITION dump payloads are byte-identical across engines")

    hgetall_len = {}
    for name, client in (("fr", fr), ("redis", redis)):
        hgetall_len[name] = len(client.command(b"HGETALL", key))
    if hgetall_len["fr"] != hgetall_len["redis"]:
        print(f"PRECONDITION FAILED: HGETALL reply sizes differ "
              f"(fr={hgetall_len['fr']} redis={hgetall_len['redis']}); the two "
              f"engines would not be doing the same work.")
        return 2
    print(f"PRECONDITION hgetall_reply_bytes={hgetall_len['fr']} identical across engines")

    targets = [
        ("fr", args.fr_port, args.fr_pid, fr),
        ("redis", args.redis_port, args.redis_pid, redis),
    ]
    # Sweeping reads-per-RESTORE locates the crossover directly instead of
    # extrapolating it from the one-read point.
    plan = [("RESTORE_only", 0), ("RESTORE_plus_1x_HGETALL", 1),
            ("RESTORE_plus_2x_HGETALL", 2), ("RESTORE_plus_4x_HGETALL", 4)]
    for workload, reads in plan:
        samples: dict[str, list[dict]] = {"fr": [], "redis": []}
        for rep in range(args.reps):
            # Alternate engine order every rep so drift cannot favour one arm.
            ordered = targets if rep % 2 == 0 else list(reversed(targets))
            for name, port, pid, client in ordered:
                result = measure(name, port, pid, payloads[name], args.ops,
                                 reads, hgetall_len[name], client)
                samples[name].append(result)
                print(f"  rep={rep} {workload} {name} "
                      f"instr_per_op={result['instr_per_op']:.1f} "
                      f"wall_ns_per_op={result['wall_ns_per_op']:.1f}")
        fr_instr = median(s["instr_per_op"] for s in samples["fr"])
        rd_instr = median(s["instr_per_op"] for s in samples["redis"])
        fr_wall = median(s["wall_ns_per_op"] for s in samples["fr"])
        rd_wall = median(s["wall_ns_per_op"] for s in samples["redis"])
        print(f"RESULT workload={workload} ops={args.ops} reps={args.reps} "
              f"fields={args.fields} value_len={args.value_len}")
        print(f"  instr_per_op   fr={fr_instr:10.1f} redis={rd_instr:10.1f} "
              f"fr_over_redis={fr_instr / rd_instr:.4f}x  (>1 means fr does MORE work)")
        print(f"  wall_ns_per_op fr={fr_wall:10.1f} redis={rd_wall:10.1f} "
              f"fr_over_redis={fr_wall / rd_wall:.4f}x  (>1 means fr is SLOWER)")

    return 0


if __name__ == "__main__":
    sys.exit(main())
