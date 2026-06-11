#!/usr/bin/env python3
"""PASS 138 post-COW large-list COPY measurement harness."""

from __future__ import annotations

import argparse
import hashlib
import json
import signal
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path


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
        b = sock.recv(1)
        if not b:
            raise EOFError("server closed socket")
        data.extend(b)
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


def stop_process(proc: subprocess.Popen[bytes], sig: signal.Signals = signal.SIGTERM) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(sig)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def load_list(sock: socket.socket, list_len: int, chunk_size: int, payload: bytes) -> float:
    start = time.perf_counter()
    send(sock, b"DEL", b"blist", b"cp")
    remaining = list_len
    while remaining:
        n = min(chunk_size, remaining)
        reply = send(sock, b"RPUSH", b"blist", *([payload] * n))
        if not isinstance(reply, int):
            raise RuntimeError(f"unexpected RPUSH reply: {reply!r}")
        remaining -= n
    return time.perf_counter() - start


def run_one_op(sock: socket.socket, mode: str, mutate_payload: bytes, range_stop: bytes) -> int:
    if mode == "copy_only":
        if send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
            raise RuntimeError("COPY failed")
        return 1
    if mode == "copy_lset_dst":
        if send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
            raise RuntimeError("COPY failed")
        if send(sock, b"LSET", b"cp", b"0", mutate_payload) != b"OK":
            raise RuntimeError("LSET failed")
        return 1
    if mode == "copy_lrange":
        rows = send(sock, b"LRANGE", b"cp", b"0", range_stop)
        if not isinstance(rows, list):
            raise RuntimeError(f"unexpected LRANGE reply: {rows!r}")
        return len(rows)
    raise ValueError(f"unknown mode {mode!r}")


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


def sanity(sock: socket.socket) -> dict[str, object]:
    sha = hashlib.sha256()
    decoded: dict[str, object] = {}
    for key_name, key in (("source", b"blist"), ("dest", b"cp")):
        llen, raw = send_recorded(sock, b"LLEN", key)
        sha.update(raw)
        head, raw = send_recorded(sock, b"LRANGE", key, b"0", b"2")
        sha.update(raw)
        encoding, raw = send_recorded(sock, b"OBJECT", b"ENCODING", key)
        sha.update(raw)
        decoded[key_name] = {
            "llen": llen,
            "lrange_0_2": [
                item.decode("latin1") if isinstance(item, bytes) else item for item in head
            ],
            "object_encoding": encoding.decode("latin1")
            if isinstance(encoding, bytes)
            else encoding,
        }
    decoded["raw_transcript_sha256"] = sha.hexdigest()
    return decoded


def measure_workload(args: argparse.Namespace, mode: str, ops: int, warmup_ops: int, range_len: int) -> dict[str, object]:
    server_log = args.out_dir / f"{args.artifact_prefix}-{mode}.server.log"
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(args.socket_timeout)
        payload = b"x" * args.payload_size
        mutate_payload = b"y" * args.payload_size
        range_stop = b"-1" if range_len <= 0 else str(range_len - 1).encode()
        setup_sec = load_list(sock, args.list_len, args.chunk_size, payload)
        if mode == "copy_lrange" and send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
            raise RuntimeError("initial COPY failed")
        for _ in range(warmup_ops):
            run_one_op(sock, mode, mutate_payload, range_stop)

        trials = []
        for trial_idx in range(args.trials):
            start = time.perf_counter()
            returned = 0
            for _ in range(ops):
                returned += run_one_op(sock, mode, mutate_payload, range_stop)
            elapsed = time.perf_counter() - start
            trials.append(
                {
                    "trial": trial_idx + 1,
                    "elapsed_sec": elapsed,
                    "ops_per_sec": ops / elapsed,
                    "per_op_us": elapsed * 1_000_000.0 / ops,
                    "items_returned": returned,
                }
            )

        behavior = sanity(sock)
        send(sock, b"QUIT")
        per_op = sorted(t["per_op_us"] for t in trials)
        ops_per_sec = [t["ops_per_sec"] for t in trials]
        return {
            "mode": mode,
            "list_len": args.list_len,
            "payload_size": args.payload_size,
            "ops_per_trial": ops,
            "warmup_ops": warmup_ops,
            "trials": trials,
            "range_len": range_len,
            "setup_sec": setup_sec,
            "summary": {
                "mean_ops_per_sec": statistics.fmean(ops_per_sec),
                "median_ops_per_sec": statistics.median(ops_per_sec),
                "p50_per_op_us": percentile(per_op, 0.50),
                "p95_per_op_us": percentile(per_op, 0.95),
                "p99_per_op_us": percentile(per_op, 0.99),
                "min_per_op_us": per_op[0],
                "max_per_op_us": per_op[-1],
            },
            "behavior": behavior,
            "server_log": str(server_log),
        }
    finally:
        stop_process(server)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--artifact-prefix", default="fresh")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=6420)
    parser.add_argument("--list-len", type=int, default=50_000)
    parser.add_argument("--payload-size", type=int, default=8)
    parser.add_argument("--chunk-size", type=int, default=1_000)
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--copy-ops", type=int, default=5_000)
    parser.add_argument("--copy-warmup", type=int, default=200)
    parser.add_argument("--lset-ops", type=int, default=5_000)
    parser.add_argument("--lset-warmup", type=int, default=100)
    parser.add_argument("--lrange-ops", type=int, default=20)
    parser.add_argument("--lrange-warmup", type=int, default=2)
    parser.add_argument("--lrange-len", type=int, default=0)
    parser.add_argument("--socket-timeout", type=float, default=120.0)
    args = parser.parse_args()

    args.out_dir.mkdir(parents=True, exist_ok=True)
    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    workloads = [
        ("copy_only", args.copy_ops, args.copy_warmup, 0),
        ("copy_lset_dst", args.lset_ops, args.lset_warmup, 0),
        ("copy_lrange", args.lrange_ops, args.lrange_warmup, args.lrange_len),
    ]
    result = {
        "started_utc": started,
        "server_bin": args.server_bin,
        "parameters": {
            "list_len": args.list_len,
            "payload_size": args.payload_size,
            "trials": args.trials,
        },
        "workloads": {},
    }
    for mode, ops, warmup, range_len in workloads:
        result["workloads"][mode] = measure_workload(args, mode, ops, warmup, range_len)
    result["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    out_path = args.out_dir / f"{args.artifact_prefix}-baseline-summary.json"
    out_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
