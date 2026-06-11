#!/usr/bin/env python3
"""Benchmark and golden-capture EVAL loop workloads for frankenredis-iror0."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path


SCRIPTS = {
    "trivial": b"return 1",
    "loop1000": b"local x=0 for i=1,1000 do x=x+i end return x",
    "table200": (
        b"local t={} for i=1,200 do t[i]=i end "
        b"local x=0 for i=1,200 do x=x+t[i] end return x"
    ),
    "closures": (
        b"local t={} for i=1,3 do t[i]=function() return i end end "
        b"return {t[1](),t[2](),t[3]()}"
    ),
}


def resp_command(*parts: bytes) -> bytes:
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def recv_line(sock: socket.socket) -> bytes:
    buf = bytearray()
    while True:
        chunk = sock.recv(1)
        if not chunk:
            raise EOFError("server closed connection")
        buf.extend(chunk)
        if buf.endswith(b"\r\n"):
            return bytes(buf)


def recv_exact(sock: socket.socket, size: int) -> bytes:
    buf = bytearray()
    while len(buf) < size:
        chunk = sock.recv(size - len(buf))
        if not chunk:
            raise EOFError("server closed connection")
        buf.extend(chunk)
    return bytes(buf)


def recv_resp_raw(sock: socket.socket) -> bytes:
    line = recv_line(sock)
    prefix = line[:1]
    if prefix == b"$":
        size = int(line[1:-2])
        if size < 0:
            return line
        return line + recv_exact(sock, size + 2)
    if prefix == b"*":
        count = int(line[1:-2])
        if count < 0:
            return line
        out = bytearray(line)
        for _ in range(count):
            out.extend(recv_resp_raw(sock))
        return bytes(out)
    return line


def wait_for_server(port: int, deadline: float) -> None:
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.1) as sock:
                sock.sendall(resp_command(b"PING"))
                if recv_resp_raw(sock) == b"+PONG\r\n":
                    return
        except OSError:
            time.sleep(0.02)
    raise TimeoutError(f"server on port {port} did not become ready")


def start_server(server_bin: str, port: int) -> subprocess.Popen[bytes]:
    env = os.environ.copy()
    env.setdefault("RUST_BACKTRACE", "0")
    proc = subprocess.Popen(
        [server_bin, "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    wait_for_server(port, time.time() + 5.0)
    return proc


def run_workload(port: int, mode: str, iterations: int) -> dict[str, object]:
    command = resp_command(b"EVAL", SCRIPTS[mode], b"0")
    digest = hashlib.sha256()
    started = time.perf_counter()
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        for _ in range(iterations):
            sock.sendall(command)
            digest.update(recv_resp_raw(sock))
    elapsed = time.perf_counter() - started
    return {
        "mode": mode,
        "iterations": iterations,
        "elapsed_seconds": elapsed,
        "ops_per_second": iterations / elapsed,
        "response_sha256": digest.hexdigest(),
    }


def run_golden(port: int) -> dict[str, object]:
    commands = [
        ("trivial", resp_command(b"EVAL", SCRIPTS["trivial"], b"0")),
        ("loop1000", resp_command(b"EVAL", SCRIPTS["loop1000"], b"0")),
        ("table200", resp_command(b"EVAL", SCRIPTS["table200"], b"0")),
        ("closures", resp_command(b"EVAL", SCRIPTS["closures"], b"0")),
    ]
    transcript: list[dict[str, str]] = []
    digest = hashlib.sha256()
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        for name, command in commands:
            sock.sendall(command)
            raw = recv_resp_raw(sock)
            digest.update(name.encode() + b"\0" + raw)
            transcript.append({"name": name, "raw_hex": raw.hex()})
    return {"sha256": digest.hexdigest(), "transcript": transcript}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=sorted(SCRIPTS), default="loop1000")
    parser.add_argument("--iterations", type=int, default=2000)
    parser.add_argument("--json-out")
    parser.add_argument("--golden-out")
    args = parser.parse_args()

    proc = start_server(args.server_bin, args.port)
    try:
        result = run_workload(args.port, args.mode, args.iterations)
        if args.golden_out:
            golden = run_golden(args.port)
            result["golden"] = golden
            with Path(args.golden_out).open("w", encoding="utf-8") as golden_file:
                golden_file.write(json.dumps(golden, indent=2, sort_keys=True) + "\n")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3.0)

    payload = json.dumps(result, indent=2, sort_keys=True)
    if args.json_out:
        with Path(args.json_out).open("w", encoding="utf-8") as json_file:
            json_file.write(payload + "\n")
    print(payload)
    return 0


if __name__ == "__main__":
    sys.exit(main())
