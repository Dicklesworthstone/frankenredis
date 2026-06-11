#!/usr/bin/env python3
"""Drive SINTERCARD LIMIT workloads against a fresh FrankenRedis server."""

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


def load_sets(sock: socket.socket, set_size: int, chunk_size: int) -> None:
    send(sock, b"DEL", b"sa", b"sb", b"missing")
    inserted = 0
    while inserted < set_size:
        n = min(chunk_size, set_size - inserted)
        members = [str(i).encode() for i in range(inserted, inserted + n)]
        if send(sock, b"SADD", b"sa", *members) != n:
            raise RuntimeError("unexpected SADD sa count")
        if send(sock, b"SADD", b"sb", *members) != n:
            raise RuntimeError("unexpected SADD sb count")
        inserted += n


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
    parser.add_argument("--set-size", type=int, default=100_000)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--ops", type=int, default=1_000)
    parser.add_argument("--warmup-ops", type=int, default=20)
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
        sock.settimeout(60)
        load_sets(sock, args.set_size, args.chunk_size)

        limit_arg = str(args.limit).encode()
        for _ in range(args.warmup_ops):
            reply = send(sock, b"SINTERCARD", b"2", b"sa", b"sb", b"LIMIT", limit_arg)
            if reply != args.limit:
                raise RuntimeError(f"unexpected warmup SINTERCARD reply: {reply!r}")

        perf = maybe_start_perf(args, server.pid)
        if perf is not None:
            time.sleep(0.1)

        start = time.perf_counter()
        last_reply = None
        for _ in range(args.ops):
            last_reply = send(sock, b"SINTERCARD", b"2", b"sa", b"sb", b"LIMIT", limit_arg)
        elapsed = time.perf_counter() - start

        full_count = send(sock, b"SINTERCARD", b"2", b"sa", b"sb")
        missing_count = send(sock, b"SINTERCARD", b"2", b"sa", b"missing", b"LIMIT", limit_arg)
        scard_a = send(sock, b"SCARD", b"sa")
        scard_b = send(sock, b"SCARD", b"sb")
        send(sock, b"QUIT")

        result = {
            "mode": "sintercard_limit",
            "set_size": args.set_size,
            "limit": args.limit,
            "ops": args.ops,
            "elapsed_sec": elapsed,
            "ops_per_sec": args.ops / elapsed,
            "last_reply": last_reply,
            "full_count": full_count,
            "missing_count": missing_count,
            "scard_a": scard_a,
            "scard_b": scard_b,
            "server_pid": server.pid,
        }
        Path(args.json_out).write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
    finally:
        if perf is not None:
            stop_process(perf, signal.SIGINT)
            try:
                _, err = perf.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                perf.kill()
                _, err = perf.communicate(timeout=5)
            if args.perf_data and args.perf_report:
                write_perf_report(args.perf_data, args.perf_report)
            if err and args.perf_data:
                Path(args.perf_data + ".stderr").write_bytes(err)
        stop_process(server)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
