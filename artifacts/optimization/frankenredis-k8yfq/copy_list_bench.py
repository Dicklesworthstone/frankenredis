#!/usr/bin/env python3
"""Benchmark COPY of a large list against a fresh FrankenRedis server."""

from __future__ import annotations

import argparse
import json
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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--list-len", type=int, default=50_000)
    parser.add_argument("--payload-size", type=int, default=8)
    parser.add_argument("--copies", type=int, default=1_000)
    parser.add_argument("--warmup-copies", type=int, default=20)
    parser.add_argument("--chunk-size", type=int, default=1_000)
    parser.add_argument("--json-out", required=True)
    args = parser.parse_args()

    server_log = Path(args.json_out).with_suffix(".server.log")
    with server_log.open("wb") as log:
        proc = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )

    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(10)
        payload = b"x" * args.payload_size

        send(sock, b"DEL", b"blist", b"cp")
        remaining = args.list_len
        while remaining:
            n = min(args.chunk_size, remaining)
            reply = send(sock, b"RPUSH", b"blist", *([payload] * n))
            if not isinstance(reply, int):
                raise RuntimeError(f"unexpected RPUSH reply: {reply!r}")
            remaining -= n

        for _ in range(args.warmup_copies):
            if send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
                raise RuntimeError("warmup COPY failed")

        start = time.perf_counter()
        for _ in range(args.copies):
            if send(sock, b"COPY", b"blist", b"cp", b"REPLACE") != 1:
                raise RuntimeError("COPY failed")
        elapsed = time.perf_counter() - start

        lrange = send(sock, b"LRANGE", b"cp", b"0", b"2")
        llen = send(sock, b"LLEN", b"cp")
        encoding = send(sock, b"OBJECT", b"ENCODING", b"cp")
        send(sock, b"QUIT")

        result = {
            "list_len": args.list_len,
            "payload_size": args.payload_size,
            "copies": args.copies,
            "copy_elapsed_sec": elapsed,
            "copy_ops_per_sec": args.copies / elapsed,
            "llen": llen,
            "lrange_head": [item.decode("latin1") for item in lrange],
            "object_encoding": encoding.decode("latin1") if isinstance(encoding, bytes) else encoding,
        }
        Path(args.json_out).write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        print(json.dumps(result, sort_keys=True))
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)


if __name__ == "__main__":
    sys.exit(main())
