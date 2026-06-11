#!/usr/bin/env python3
"""Drive post-COW large-list workloads against a fresh FrankenRedis server."""

from __future__ import annotations

import argparse
import json
import signal
import socket
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


def read_line(sock: socket.socket) -> bytes:
    data = bytearray()
    while True:
        b = sock.recv(1)
        if not b:
            raise EOFError("server closed socket")
        data.extend(b)
        if data.endswith(b"\r\n"):
            return bytes(data[:-2])


def read_resp(sock: socket.socket):
    prefix = sock.recv(1)
    if not prefix:
        raise EOFError("server closed socket")
    if prefix in (b"+", b"-", b":"):
        line = read_line(sock)
        if prefix == b"-":
            raise RuntimeError(line.decode("utf-8", "replace"))
        if prefix == b":":
            return int(line)
        return line
    if prefix == b"$":
        n = int(read_line(sock))
        if n < 0:
            return None
        body = bytearray()
        while len(body) < n + 2:
            chunk = sock.recv(n + 2 - len(body))
            if not chunk:
                raise EOFError("server closed socket")
            body.extend(chunk)
        return bytes(body[:-2])
    if prefix == b"*":
        n = int(read_line(sock))
        if n < 0:
            return None
        return [read_resp(sock) for _ in range(n)]
    raise RuntimeError(f"unexpected RESP prefix {prefix!r}")


def send(sock: socket.socket, *parts: bytes):
    sock.sendall(resp_command(*parts))
    return read_resp(sock)


def wait_for_server(host: str, port: int, deadline: float) -> socket.socket:
    last_error: OSError | None = None
    while time.time() < deadline:
        try:
            return socket.create_connection((host, port), timeout=0.25)
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise TimeoutError(f"server did not accept connections: {last_error}")


def load_list(sock: socket.socket, list_len: int, chunk_size: int, payload: bytes) -> None:
    send(sock, b"DEL", b"blist", b"cp")
    remaining = list_len
    while remaining:
        n = min(chunk_size, remaining)
        reply = send(sock, b"RPUSH", b"blist", *([payload] * n))
        if not isinstance(reply, int):
            raise RuntimeError(f"unexpected RPUSH reply: {reply!r}")
        remaining -= n


def run_op(sock: socket.socket, mode: str, mutate_payload: bytes, range_stop: bytes) -> int:
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


def stop_process(proc: subprocess.Popen[bytes], sig: signal.Signals = signal.SIGTERM) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(sig)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def maybe_start_perf(args: argparse.Namespace, server_pid: int) -> subprocess.Popen[bytes] | None:
    if not args.perf_data:
        return None
    return subprocess.Popen(
        [
            "perf",
            "record",
            "-e",
            args.perf_event,
            "-F",
            str(args.perf_freq),
            "-g",
            "-p",
            str(server_pid),
            "-o",
            args.perf_data,
            "--",
            "sleep",
            "600",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def write_perf_report(perf_data: str, perf_report: str | None) -> None:
    if not perf_report:
        return
    with Path(perf_report).open("wb") as out:
        subprocess.run(
            [
                "perf",
                "report",
                "--stdio",
                "--no-children",
                "--sort",
                "symbol",
                "-i",
                perf_data,
            ],
            check=False,
            stdout=out,
            stderr=subprocess.STDOUT,
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=["copy_only", "copy_lset_dst", "copy_lrange"], required=True)
    parser.add_argument("--list-len", type=int, default=50_000)
    parser.add_argument("--payload-size", type=int, default=8)
    parser.add_argument("--ops", type=int, default=1_000)
    parser.add_argument("--warmup-ops", type=int, default=20)
    parser.add_argument("--range-len", type=int, default=5_000)
    parser.add_argument("--chunk-size", type=int, default=1_000)
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--perf-data")
    parser.add_argument("--perf-report")
    parser.add_argument("--perf-event", default="cycles:u")
    parser.add_argument("--perf-freq", type=int, default=997)
    args = parser.parse_args()

    server_log = Path(args.json_out).with_suffix(".server.log")
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )

    perf: subprocess.Popen[bytes] | None = None
    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(30)
        payload = b"x" * args.payload_size
        mutate_payload = b"y" * args.payload_size
        range_stop = b"-1" if args.range_len <= 0 else str(args.range_len - 1).encode()

        load_list(sock, args.list_len, args.chunk_size, payload)
        if args.mode == "copy_lrange" and send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
            raise RuntimeError("initial COPY failed")

        for _ in range(args.warmup_ops):
            run_op(sock, args.mode, mutate_payload, range_stop)

        perf = maybe_start_perf(args, server.pid)
        if perf is not None:
            time.sleep(0.1)

        total_items = 0
        start = time.perf_counter()
        for _ in range(args.ops):
            total_items += run_op(sock, args.mode, mutate_payload, range_stop)
        elapsed = time.perf_counter() - start

        lrange = send(sock, b"LRANGE", b"cp", b"0", b"2")
        llen = send(sock, b"LLEN", b"cp")
        encoding = send(sock, b"OBJECT", b"ENCODING", b"cp")
        send(sock, b"QUIT")

        result = {
            "mode": args.mode,
            "list_len": args.list_len,
            "payload_size": args.payload_size,
            "ops": args.ops,
            "elapsed_sec": elapsed,
            "ops_per_sec": args.ops / elapsed,
            "items_returned": total_items,
            "range_len": args.range_len,
            "llen": llen,
            "lrange_head": [item.decode("latin1") for item in lrange],
            "object_encoding": encoding.decode("latin1") if isinstance(encoding, bytes) else encoding,
            "server_pid": server.pid,
        }
        Path(args.json_out).write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        print(json.dumps(result, sort_keys=True))
        return 0
    finally:
        if perf is not None:
            stop_process(perf, signal.SIGINT)
            if perf.stderr is not None:
                err = perf.stderr.read()
                if err:
                    Path(args.json_out).with_suffix(".perf.stderr").write_bytes(err)
            if args.perf_data:
                write_perf_report(args.perf_data, args.perf_report)
        stop_process(server)


if __name__ == "__main__":
    sys.exit(main())
