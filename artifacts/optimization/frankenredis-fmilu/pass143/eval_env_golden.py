#!/usr/bin/env python3
"""Golden RESP transcript for frankenredis-fmilu Lua environment sharing."""

from __future__ import annotations

import argparse
import hashlib
import json
import signal
import socket
import subprocess
import time
from pathlib import Path


CASES = [
    (b"EVAL", b"return type(_G)..':'..tostring(_G._G == _G)..':'..type(_G.redis)..':'..type(_G.math)", b"0"),
    (b"EVAL", b"local out={}; for k,v in pairs(_G) do table.insert(out,k) end; return table.concat(out, ',')", b"0"),
    (b"EVAL", b"return tostring(getfenv(0) == _G)", b"0"),
    (b"EVAL", b"local ok,e=pcall(function() rawset(redis,'x',1) end); return tostring(ok)..':'..tostring(e)", b"0"),
    (b"EVAL", b"return redis.REDIS_VERSION..':'..tostring(redis.REPL_ALL)..':'..type(redis.call)", b"0"),
    (b"EVAL", b"return KEYS[1]..':'..ARGV[1]", b"1", b"k", b"v"),
    (b"EVAL", b"math.randomseed(1); return tostring(math.random())..':'..tostring(math.random(1,10))", b"0"),
    (b"EVAL", b"local c=coroutine.create(function() coroutine.yield('x'); return 'y' end); local ok,v=coroutine.resume(c); return tostring(ok)..':'..v..':'..coroutine.status(c)", b"0"),
]


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


def render(value: object) -> object:
    if isinstance(value, bytes):
        return value.decode("latin1")
    if isinstance(value, list):
        return [render(item) for item in value]
    return value


def run(args: argparse.Namespace) -> dict[str, object]:
    args.out_dir.mkdir(parents=True, exist_ok=True)
    server_log = args.out_dir / f"{args.artifact_prefix}.env-golden.server.log"
    with server_log.open("wb") as log:
        server = subprocess.Popen(
            [args.server_bin, "--bind", args.host, "--port", str(args.port)],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    try:
        sock = wait_for_server(args.host, args.port, time.time() + 5)
        sock.settimeout(args.socket_timeout)
        sha = hashlib.sha256()
        replies = []
        for parts in [(b"FLUSHDB",), *CASES, (b"PING",)]:
            value, raw = send_recorded(sock, *parts)
            sha.update(raw)
            replies.append({"argv": [p.decode("latin1") for p in parts], "reply": render(value)})
        send_recorded(sock, b"QUIT")
        return {
            "case_count": len(CASES),
            "raw_transcript_sha256": sha.hexdigest(),
            "replies": replies,
            "server_log": str(server_log),
        }
    finally:
        stop_process(server)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--artifact-prefix", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--socket-timeout", type=float, default=10.0)
    args = parser.parse_args()
    result = run(args)
    out = args.out_dir / f"{args.artifact_prefix}.env-golden.json"
    out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"raw_transcript_sha256": result["raw_transcript_sha256"]}, sort_keys=True))
    print(f"summary_json={out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
