#!/usr/bin/env python3
"""Benchmark and golden comparator for frankenredis-jjg2q.

The workload matches the bead's /tmp/zperf.py shape: 20k-member zsets with
overlap, then repeated ZINTERSTORE/ZDIFFSTORE/ZUNIONSTORE/ZINTERCARD commands.
It deliberately uses a tiny RESP client so no Python Redis package is required.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import socket
import statistics
import time
from typing import Any


CASES = {
    "zinter2": ("ZINTERSTORE", "d", "2", "za", "zb"),
    "zinter3": ("ZINTERSTORE", "d", "3", "za", "zb", "zc"),
    "zdiff2": ("ZDIFFSTORE", "d", "2", "za", "zb"),
    "zunion2": ("ZUNIONSTORE", "d", "2", "za", "zb"),
    "zintercard2": ("ZINTERCARD", "2", "za", "zb"),
}


class Conn:
    def __init__(self, port: int) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), 3)
        self.sock.settimeout(60)
        self.buf = b""

    def close(self) -> None:
        self.sock.close()

    def _line(self) -> bytes:
        while b"\r\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _bytes(self, count: int) -> bytes:
        while len(self.buf) < count + 2:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("connection closed")
            self.buf += chunk
        data, self.buf = self.buf[:count], self.buf[count + 2 :]
        return data

    def parse(self) -> Any:
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag == b"$":
            count = int(rest)
            if count < 0:
                return {"bulk": None}
            return {"bulk": self._bytes(count).hex()}
        if tag == b":":
            return {"int": int(rest)}
        if tag == b"+":
            return {"status": rest.decode("utf-8", "replace")}
        if tag == b"-":
            return {"error": rest.decode("utf-8", "replace")}
        if tag == b"*":
            count = int(rest)
            if count < 0:
                return {"array": None}
            return {"array": [self.parse() for _ in range(count)]}
        raise ValueError(f"unknown RESP tag {tag!r}")

    def cmd(self, *args: object) -> Any:
        parts = [f"*{len(args)}\r\n".encode()]
        for arg in args:
            data = arg if isinstance(arg, bytes) else str(arg).encode()
            parts.append(b"$%d\r\n%s\r\n" % (len(data), data))
        self.sock.sendall(b"".join(parts))
        return self.parse()


def zadd_range(conn: Conn, key: str, values: range, chunk_size: int) -> None:
    pending: list[object] = []
    for i in values:
        pending.extend((str(i), f"m{i}"))
        if len(pending) >= chunk_size * 2:
            conn.cmd("ZADD", key, *pending)
            pending.clear()
    if pending:
        conn.cmd("ZADD", key, *pending)


def seed_large(conn: Conn, n: int, chunk_size: int) -> None:
    conn.cmd("FLUSHALL")
    zadd_range(conn, "za", range(0, n), chunk_size)
    zadd_range(conn, "zb", range(n // 2, n + n // 2), chunk_size)
    zadd_range(conn, "zc", range(0, n + n // 2, 2), chunk_size)


def run_case(conn: Conn, case: str, iters: int) -> tuple[Any, float]:
    command = CASES[case]
    reply = conn.cmd(*command)
    start = time.perf_counter()
    for _ in range(iters):
        reply = conn.cmd(*command)
    elapsed = time.perf_counter() - start
    return reply, elapsed


def one(args: argparse.Namespace) -> None:
    conn = Conn(args.port)
    try:
        seed_large(conn, args.n, args.chunk_size)
        reply, elapsed = run_case(conn, args.case, args.iters)
        print(
            json.dumps(
                {
                    "case": args.case,
                    "iters": args.iters,
                    "elapsed_s": elapsed,
                    "us_per_op": elapsed / args.iters * 1_000_000.0,
                    "reply": reply,
                },
                sort_keys=True,
            )
        )
    finally:
        conn.close()


def median_us(conn: Conn, case: str, iters: int, repeats: int) -> tuple[Any, float]:
    reply: Any = None
    samples = []
    for _ in range(repeats):
        reply, elapsed = run_case(conn, case, iters)
        samples.append(elapsed / iters * 1_000_000.0)
    return reply, statistics.median(samples)


def compare(args: argparse.Namespace) -> None:
    ports = {
        "baseline": args.baseline_port,
        "candidate": args.candidate_port,
        "redis": args.redis_port,
    }
    conns = {name: Conn(port) for name, port in ports.items()}
    try:
        for conn in conns.values():
            seed_large(conn, args.n, args.chunk_size)
        rows = []
        for case in args.compare_cases:
            replies = {}
            timings = {}
            for name, conn in conns.items():
                reply, us = median_us(conn, case, args.iters, args.repeats)
                replies[name] = reply
                timings[name] = us
            row = {
                "case": case,
                "us_per_op": timings,
                "reply_equal": replies["baseline"] == replies["candidate"] == replies["redis"],
                "candidate_vs_baseline": timings["baseline"] / timings["candidate"],
                "candidate_vs_redis": timings["redis"] / timings["candidate"],
                "baseline_vs_redis": timings["redis"] / timings["baseline"],
            }
            rows.append(row)
            print(json.dumps(row, sort_keys=True))
        print(json.dumps({"rows": rows}, sort_keys=True))
    finally:
        for conn in conns.values():
            conn.close()


def seed_golden(conn: Conn) -> None:
    conn.cmd("FLUSHALL")
    conn.cmd("ZADD", "z", "1", "a", "2", "b", "3", "c")
    conn.cmd("ZADD", "other", "10", "a", "20", "c")
    conn.cmd("ZADD", "ties1", "1", "a", "1", "b", "1", "c")
    conn.cmd("ZADD", "ties2", "1", "b", "1", "a")
    conn.cmd("SADD", "s", "a", "c")
    conn.cmd("SET", "str", "wrong")


def transcript(conn: Conn) -> list[dict[str, Any]]:
    seed_golden(conn)
    cases: list[tuple[str, tuple[object, ...], tuple[object, ...] | None]] = [
        (
            "dest-source-duplicates",
            ("ZINTERSTORE", "z", "3", "z", "other", "z", "WEIGHTS", "2", "3", "5"),
            ("ZRANGE", "z", "0", "-1", "WITHSCORES"),
        ),
        (
            "tie-order",
            ("ZINTERSTORE", "d", "2", "ties1", "ties2"),
            ("ZRANGE", "d", "0", "-1", "WITHSCORES"),
        ),
        (
            "set-input",
            ("ZINTERSTORE", "ds", "2", "ties1", "s"),
            ("ZRANGE", "ds", "0", "-1", "WITHSCORES"),
        ),
        (
            "missing-source",
            ("ZINTERSTORE", "dm", "2", "ties1", "missing"),
            ("EXISTS", "dm"),
        ),
        (
            "wrong-type",
            ("ZINTERSTORE", "dw", "2", "ties1", "str"),
            None,
        ),
    ]
    output = []
    for label, command, verify in cases:
        reply = conn.cmd(*command)
        verify_reply = conn.cmd(*verify) if verify is not None else None
        output.append(
            {
                "label": label,
                "command": command,
                "reply": reply,
                "verify": verify,
                "verify_reply": verify_reply,
            }
        )
    return output


def golden(args: argparse.Namespace) -> None:
    ports = {
        "baseline": args.baseline_port,
        "candidate": args.candidate_port,
        "redis": args.redis_port,
    }
    outputs = {}
    for name, port in ports.items():
        conn = Conn(port)
        try:
            outputs[name] = transcript(conn)
        finally:
            conn.close()
    payload = json.dumps(outputs, sort_keys=True, separators=(",", ":")).encode()
    digest = hashlib.sha256(payload).hexdigest()
    equal = outputs["baseline"] == outputs["candidate"] == outputs["redis"]
    print(json.dumps({"sha256": digest, "equal": equal, "outputs": outputs}, sort_keys=True))
    if not equal:
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int)
    parser.add_argument("--baseline-port", type=int)
    parser.add_argument("--candidate-port", type=int)
    parser.add_argument("--redis-port", type=int)
    parser.add_argument("--case", choices=sorted(CASES), default="zinter2")
    parser.add_argument("--compare-cases", nargs="+", choices=sorted(CASES), default=sorted(CASES))
    parser.add_argument("--iters", type=int, default=400)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--n", type=int, default=20_000)
    parser.add_argument("--chunk-size", type=int, default=1_000)
    parser.add_argument("--compare", action="store_true")
    parser.add_argument("--golden", action="store_true")
    args = parser.parse_args()

    if args.golden:
        if args.baseline_port is None or args.candidate_port is None or args.redis_port is None:
            parser.error("--golden requires --baseline-port, --candidate-port, and --redis-port")
        golden(args)
    elif args.compare:
        if args.baseline_port is None or args.candidate_port is None or args.redis_port is None:
            parser.error("--compare requires --baseline-port, --candidate-port, and --redis-port")
        compare(args)
    else:
        if args.port is None:
            parser.error("single-case mode requires --port")
        one(args)


if __name__ == "__main__":
    main()
