#!/usr/bin/env python3
"""Drive PFCOUNT workloads against a fresh FrankenRedis server."""

from __future__ import annotations

import argparse
import json
import signal
import socket
import subprocess
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
        chunk = sock.recv(1)
        if not chunk:
            raise EOFError("server closed socket")
        data.extend(chunk)
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
        data = bytearray()
        while len(data) < n + 2:
            chunk = sock.recv(n + 2 - len(data))
            if not chunk:
                raise EOFError("server closed socket")
            data.extend(chunk)
        return bytes(data[:-2])
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


def stop_process(proc: subprocess.Popen[bytes], sig: signal.Signals = signal.SIGTERM) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(sig)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def load_hll(sock: socket.socket, key: bytes, start: int, count: int, chunk_size: int) -> None:
    inserted = 0
    while inserted < count:
        n = min(chunk_size, count - inserted)
        elements = [f"m{start + inserted + i}".encode() for i in range(n)]
        send(sock, b"PFADD", key, *elements)
        inserted += n


def command_for_mode(mode: str) -> tuple[bytes, ...]:
    if mode == "single-cached":
        return (b"PFCOUNT", b"hllA")
    if mode == "multi":
        return (b"PFCOUNT", b"hllA", b"hllB")
    raise ValueError(f"unsupported mode {mode!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=("single-cached", "multi"), required=True)
    parser.add_argument("--elements", type=int, default=50_000)
    parser.add_argument("--ops", type=int, default=20_000)
    parser.add_argument("--warmup-ops", type=int, default=100)
    parser.add_argument("--chunk-size", type=int, default=1_000)
    parser.add_argument("--json-out", required=True)
    args = parser.parse_args()

    server_log = Path(args.json_out).with_suffix(".server.log")
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )

    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(60)
        send(sock, b"DEL", b"hllA", b"hllB")
        load_hll(sock, b"hllA", 0, args.elements, args.chunk_size)
        load_hll(sock, b"hllB", args.elements, args.elements, args.chunk_size)

        single_count = send(sock, b"PFCOUNT", b"hllA")
        other_count = send(sock, b"PFCOUNT", b"hllB")
        merged_count = send(sock, b"PFCOUNT", b"hllA", b"hllB")
        command = command_for_mode(args.mode)
        for _ in range(args.warmup_ops):
            send(sock, *command)

        start = time.perf_counter()
        last_reply = None
        for _ in range(args.ops):
            last_reply = send(sock, *command)
        elapsed = time.perf_counter() - start
        send(sock, b"QUIT")

        result = {
            "mode": args.mode,
            "elements_per_hll": args.elements,
            "ops": args.ops,
            "elapsed_sec": elapsed,
            "ops_per_sec": args.ops / elapsed,
            "last_reply": last_reply,
            "single_count": single_count,
            "other_count": other_count,
            "merged_count": merged_count,
            "server_pid": server.pid,
        }
        Path(args.json_out).write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
    finally:
        stop_process(server)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
