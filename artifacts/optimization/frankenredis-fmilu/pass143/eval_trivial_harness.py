#!/usr/bin/env python3
"""Profile-backed trivial EVAL harness for frankenredis-fmilu."""

from __future__ import annotations

import argparse
import hashlib
import json
import signal
import socket
import statistics
import subprocess
import time
from pathlib import Path


SCRIPT = b"return 1"


def resp_command(*parts: bytes) -> bytes:
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_line_raw(sock: socket.socket) -> bytes:
    data = bytearray()
    while True:
        chunk = sock.recv(1)
        if not chunk:
            raise EOFError("server closed socket")
        data.extend(chunk)
        if data.endswith(b"\r\n"):
            return bytes(data)


def read_resp_raw(sock: socket.socket) -> tuple[object, bytes]:
    prefix = sock.recv(1)
    if not prefix:
        raise EOFError("server closed socket")
    raw = bytearray(prefix)
    if prefix in (b"+", b"-", b":"):
        line_raw = read_line_raw(sock)
        raw.extend(line_raw)
        line = line_raw[:-2]
        if prefix == b"-":
            raise RuntimeError(line.decode("utf-8", "replace"))
        if prefix == b":":
            return int(line), bytes(raw)
        return bytes(line), bytes(raw)
    if prefix == b"$":
        line_raw = read_line_raw(sock)
        raw.extend(line_raw)
        n = int(line_raw[:-2])
        if n < 0:
            return None, bytes(raw)
        body = bytearray()
        while len(body) < n + 2:
            chunk = sock.recv(n + 2 - len(body))
            if not chunk:
                raise EOFError("server closed socket")
            body.extend(chunk)
        raw.extend(body)
        return bytes(body[:-2]), bytes(raw)
    if prefix == b"*":
        line_raw = read_line_raw(sock)
        raw.extend(line_raw)
        n = int(line_raw[:-2])
        if n < 0:
            return None, bytes(raw)
        values = []
        for _ in range(n):
            value, child_raw = read_resp_raw(sock)
            values.append(value)
            raw.extend(child_raw)
        return values, bytes(raw)
    raise RuntimeError(f"unexpected RESP prefix {prefix!r}")


def send(sock: socket.socket, *parts: bytes) -> object:
    sock.sendall(resp_command(*parts))
    value, _ = read_resp_raw(sock)
    return value


def send_recorded(sock: socket.socket, *parts: bytes) -> tuple[object, bytes]:
    request = resp_command(*parts)
    sock.sendall(request)
    value, response = read_resp_raw(sock)
    return value, request + response


def wait_for_server(host: str, port: int, deadline: float) -> socket.socket:
    last_error: OSError | None = None
    while time.time() < deadline:
        try:
            return socket.create_connection((host, port), timeout=0.25)
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise TimeoutError(f"server did not accept connections: {last_error}")


def stop_process(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def percentile(sorted_values: list[float], pct: float) -> float:
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    rank = (len(sorted_values) - 1) * pct
    low = int(rank)
    high = min(low + 1, len(sorted_values) - 1)
    weight = rank - low
    return sorted_values[low] * (1.0 - weight) + sorted_values[high] * weight


def run_trial(sock: socket.socket, ops: int) -> dict[str, object]:
    sha = hashlib.sha256()
    first_values = []
    start = time.perf_counter()
    for i in range(ops):
        value, raw = send_recorded(sock, b"EVAL", SCRIPT, b"0")
        if value != 1:
            raise RuntimeError(f"unexpected EVAL reply at op {i}: {value!r}")
        if len(first_values) < 5:
            first_values.append(value)
        sha.update(raw)
    elapsed = time.perf_counter() - start
    return {
        "elapsed_sec": elapsed,
        "ops_per_sec": ops / elapsed,
        "per_op_us": elapsed * 1_000_000.0 / ops,
        "first_values": first_values,
        "raw_transcript_sha256": sha.hexdigest(),
    }


def measure(args: argparse.Namespace) -> dict[str, object]:
    args.out_dir.mkdir(parents=True, exist_ok=True)
    server_log = args.out_dir / f"{args.artifact_prefix}.server.log"
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(args.socket_timeout)
        send(sock, b"FLUSHDB")
        for _ in range(args.warmup_ops):
            value = send(sock, b"EVAL", SCRIPT, b"0")
            if value != 1:
                raise RuntimeError(f"unexpected warmup reply: {value!r}")
        trials = [run_trial(sock, args.ops) for _ in range(args.trials)]
        ping, ping_raw = send_recorded(sock, b"PING")
        send(sock, b"QUIT")
        per_op = sorted(t["per_op_us"] for t in trials)
        ops_per_sec = [t["ops_per_sec"] for t in trials]
        behavior_sha = hashlib.sha256(ping_raw).hexdigest()
        return {
            "mode": "eval_return_one",
            "script": SCRIPT.decode("ascii"),
            "ops_per_trial": args.ops,
            "warmup_ops": args.warmup_ops,
            "trials": trials,
            "summary": {
                "mean_ops_per_sec": statistics.fmean(ops_per_sec),
                "median_ops_per_sec": statistics.median(ops_per_sec),
                "p50_per_op_us": percentile(per_op, 0.50),
                "p95_per_op_us": percentile(per_op, 0.95),
                "p99_per_op_us": percentile(per_op, 0.99),
                "min_per_op_us": per_op[0],
                "max_per_op_us": per_op[-1],
            },
            "behavior": {
                "ping": ping.decode("latin1") if isinstance(ping, bytes) else ping,
                "raw_transcript_sha256": behavior_sha,
            },
            "server_log": str(server_log),
        }
    finally:
        stop_process(server)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--artifact-prefix", default="baseline")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=26471)
    parser.add_argument("--ops", type=int, default=10_000)
    parser.add_argument("--warmup-ops", type=int, default=200)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--socket-timeout", type=float, default=10.0)
    args = parser.parse_args()
    result = measure(args)
    out = args.out_dir / f"{args.artifact_prefix}.json"
    out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result["summary"], sort_keys=True))
    print(f"summary_json={out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
