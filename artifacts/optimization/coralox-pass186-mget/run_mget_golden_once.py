#!/usr/bin/env python3
import argparse
import hashlib
import socket
import subprocess  # nosec B404 - trusted local proof binary.
import sys
import time
from pathlib import Path


def resp_command(*parts: bytes) -> bytes:
    out = [b"*" + str(len(parts)).encode() + b"\r\n"]
    for part in parts:
        out.append(b"$" + str(len(part)).encode() + b"\r\n" + part + b"\r\n")
    return b"".join(out)


def build_payload() -> bytes:
    commands = [
        (b"SET", b"a", b"alpha"),
        (b"SET", b"b", b"beta"),
        (b"INCR", b"i"),
        (b"LPUSH", b"list", b"x"),
        (b"MGET", b"a", b"missing", b"i", b"list", b"b", b"a"),
        (b"CLIENT", b"REPLY", b"SKIP"),
        (b"MGET", b"a", b"b"),
        (b"MGET", b"a", b"b"),
        (b"HELLO", b"3"),
        (b"MGET", b"a", b"missing", b"list", b"i"),
        (b"CLIENT", b"REPLY", b"SKIP"),
        (b"MGET", b"a"),
        (b"MGET", b"a"),
        (b"QUIT",),
    ]
    return b"".join(resp_command(*command) for command in commands)


def wait_for_server(port: int, proc: subprocess.Popen[bytes]) -> None:
    deadline = time.time() + 5
    while True:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            if proc.poll() is not None:
                raise RuntimeError(f"server exited early with {proc.returncode}")
            if time.time() > deadline:
                raise TimeoutError("server did not accept connections")
            time.sleep(0.05)


def read_transcript(port: int, payload: bytes) -> bytes:
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        sock.settimeout(2.0)
        chunks = []
        while True:
            try:
                chunk = sock.recv(65536)
            except socket.timeout:
                break
            if not chunk:
                break
            chunks.append(chunk)
    return b"".join(chunks)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--input-out", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    payload = build_payload()
    proc = subprocess.Popen(  # nosec B603 - proof harness uses trusted local paths.
        [args.bin, "--bind", "127.0.0.1", "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        wait_for_server(args.port, proc)
        output = read_transcript(args.port, payload)
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)

    input_path = Path(args.input_out)
    output_path = Path(args.output)
    input_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    input_path.write_bytes(payload)
    output_path.write_bytes(output)
    print(f"input_sha256={hashlib.sha256(payload).hexdigest()}")
    print(f"output_sha256={hashlib.sha256(output).hexdigest()}")
    print(f"output_bytes={len(output)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
