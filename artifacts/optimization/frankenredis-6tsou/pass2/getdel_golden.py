#!/usr/bin/env python3
"""Golden transcript for the frankenredis-6tsou GETDEL memory-cache lever."""

import argparse
import hashlib
import json
import socket
import subprocess
import time
from pathlib import Path


CASES = [
    (b"FLUSHALL",),
    (b"SET", b"a", b"alpha"),
    (b"SET", b"b", b"bravo"),
    (b"GETDEL", b"a"),
    (b"GET", b"a"),
    (b"GET", b"b"),
    (b"GETDEL", b"missing"),
    (b"RPUSH", b"list", b"x"),
    (b"GETDEL", b"list"),
    (b"GET", b"list"),
]


def resp_command(parts):
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_one(sock):
    buf = bytearray()
    while True:
        chunk = sock.recv(1)
        if not chunk:
            raise RuntimeError("server closed connection")
        buf.extend(chunk)
        prefix = buf[0]
        if prefix in (43, 45, 58):
            if buf.endswith(b"\r\n"):
                return bytes(buf)
        elif prefix == 36:
            if b"\r\n" not in buf:
                continue
            header_end = buf.find(b"\r\n")
            length = int(buf[1:header_end])
            if length < 0:
                return bytes(buf)
            needed = header_end + 2 + length + 2
            while len(buf) < needed:
                more = sock.recv(needed - len(buf))
                if not more:
                    raise RuntimeError("server closed bulk reply")
                buf.extend(more)
            return bytes(buf)
        elif prefix == 42:
            raise RuntimeError("array replies are not expected")
        else:
            raise RuntimeError(f"unexpected RESP prefix {prefix!r}")


def wait_for_port(port, timeout_s=5.0):
    deadline = time.monotonic() + timeout_s
    last_error = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise RuntimeError(f"server did not open port {port}: {last_error}")


def transcript(server_bin, port):
    proc = subprocess.Popen(
        [server_bin, "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        wait_for_port(port)
        replies = []
        with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
            sock.settimeout(5.0)
            for parts in CASES:
                sock.sendall(resp_command(parts))
                replies.append(read_one(sock).decode("latin1"))
        return replies
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3.0)
        if proc.returncode not in (0, -15) and proc.stderr is not None:
            err = proc.stderr.read()
            if err:
                raise RuntimeError(err)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-bin", required=True)
    parser.add_argument("--candidate-bin", required=True)
    parser.add_argument("--baseline-port", type=int, default=16640)
    parser.add_argument("--candidate-port", type=int, default=16641)
    parser.add_argument("--json-out", required=True)
    args = parser.parse_args()

    baseline = transcript(args.baseline_bin, args.baseline_port)
    candidate = transcript(args.candidate_bin, args.candidate_port)
    if candidate != baseline:
        raise SystemExit("candidate transcript differs from baseline")

    payload = {
        "schema": "frankenredis_6tsou_getdel_golden_v1",
        "cases": [[part.decode("latin1") for part in parts] for parts in CASES],
        "sha256": hashlib.sha256("".join(candidate).encode("latin1")).hexdigest(),
        "transcript": candidate,
    }
    text = json.dumps(payload, indent=2, sort_keys=True)
    Path(args.json_out).write_text(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
