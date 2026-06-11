#!/usr/bin/env python3
"""Record a deterministic PFCOUNT behavior transcript for pass144."""

from __future__ import annotations

import argparse
import base64
import hashlib
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


def read_resp(sock: socket.socket) -> tuple[str, object]:
    prefix = sock.recv(1)
    if not prefix:
        raise EOFError("server closed socket")
    if prefix in (b"+", b"-", b":"):
        line = read_line(sock)
        if prefix == b"+":
            return ("simple", line.decode("utf-8", "replace"))
        if prefix == b"-":
            return ("error", line.decode("utf-8", "replace"))
        return ("integer", int(line))
    if prefix == b"$":
        n = int(read_line(sock))
        if n < 0:
            return ("bulk", None)
        data = bytearray()
        while len(data) < n + 2:
            chunk = sock.recv(n + 2 - len(data))
            if not chunk:
                raise EOFError("server closed socket")
            data.extend(chunk)
        payload = bytes(data[:-2])
        return (
            "bulk",
            {
                "base64": base64.b64encode(payload).decode(),
                "len": len(payload),
                "sha256": hashlib.sha256(payload).hexdigest(),
            },
        )
    if prefix == b"*":
        n = int(read_line(sock))
        if n < 0:
            return ("array", None)
        return ("array", [read_resp(sock) for _ in range(n)])
    raise RuntimeError(f"unexpected RESP prefix {prefix!r}")


def send(sock: socket.socket, *parts: bytes) -> dict[str, object]:
    sock.sendall(resp_command(*parts))
    kind, value = read_resp(sock)
    return {
        "command": [part.decode("utf-8", "replace") for part in parts],
        "kind": kind,
        "value": value,
    }


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


def hll_elements(prefix: str, start: int, count: int) -> list[bytes]:
    return [f"{prefix}{start + i}".encode() for i in range(count)]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--json-out", required=True)
    args = parser.parse_args()

    out_path = Path(args.json_out)
    server_log = out_path.with_suffix(".server.log")
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )

    transcript: list[dict[str, object]] = []
    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(60)
        commands: list[tuple[bytes, ...]] = [
            (b"DEL", b"hllA", b"hllB", b"hllM", b"plain"),
            (b"PFADD", b"hllA", *hll_elements("a", 0, 512)),
            (b"PFADD", b"hllB", *hll_elements("b", 0, 512)),
            (b"PFCOUNT", b"hllA"),
            (b"PFCOUNT", b"hllA"),
            (b"PFCOUNT", b"hllB"),
            (b"PFCOUNT", b"hllA", b"hllB"),
            (b"PFCOUNT", b"hllA", b"hllB"),
            (b"OBJECT", b"ENCODING", b"hllA"),
            (b"PFDEBUG", b"ENCODING", b"hllA"),
            (b"DUMP", b"hllA"),
            (b"PFMERGE", b"hllM", b"hllA", b"hllB"),
            (b"PFCOUNT", b"hllM"),
            (b"OBJECT", b"ENCODING", b"hllM"),
            (b"DUMP", b"hllM"),
            (b"SET", b"hllA", b"not-a-hyperloglog"),
            (b"PFCOUNT", b"hllA", b"hllB"),
            (b"PFADD", b"hllA", *hll_elements("a", 256, 256)),
            (b"PFCOUNT", b"hllA", b"hllB"),
            (b"QUIT",),
        ]
        for command in commands:
            transcript.append(send(sock, *command))
    finally:
        stop_process(server)

    raw = json.dumps(transcript, sort_keys=True, separators=(",", ":")).encode()
    result = {
        "sha256": hashlib.sha256(raw).hexdigest(),
        "transcript": transcript,
    }
    out_path.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
