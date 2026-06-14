#!/usr/bin/env python3
import argparse
import socket
import subprocess  # nosec B404 - trusted local benchmark binaries only.
import sys
import time
from pathlib import Path


def resp_command(*parts: bytes) -> bytes:
    out = [b"*" + str(len(parts)).encode() + b"\r\n"]
    for part in parts:
        out.append(b"$" + str(len(part)).encode() + b"\r\n" + part + b"\r\n")
    return b"".join(out)


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


def preload_keys(port: int, keyspace: int, value_size: int) -> None:
    value = b"x" * value_size
    payload = b"".join(
        resp_command(b"SET", f"key:{i:012d}".encode(), value) for i in range(keyspace)
    )
    with socket.create_connection(("127.0.0.1", port), timeout=10.0) as sock:
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        sock.settimeout(1.0)
        chunks = []
        while True:
            try:
                chunk = sock.recv(65536)
            except socket.timeout:
                break
            if not chunk:
                break
            chunks.append(chunk)
    replies = b"".join(chunks)
    ok_count = replies.count(b"+OK\r\n")
    if ok_count != keyspace:
        raise RuntimeError(f"preload got {ok_count} OK replies for {keyspace} keys")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    parser.add_argument("--kind", choices=["redis", "fr"], required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--requests", default=800_000, type=int)
    parser.add_argument("--clients", default=50, type=int)
    parser.add_argument("--pipeline", default=8, type=int)
    parser.add_argument("--keyspace", default=10_000, type=int)
    parser.add_argument("--value-size", default=16, type=int)
    parser.add_argument(
        "--redis-benchmark",
        default="/dp/frankenredis/legacy_redis_code/redis/src/redis-benchmark",
    )
    args = parser.parse_args()

    Path("artifacts/optimization/coralox-pass186-mget").mkdir(parents=True, exist_ok=True)
    if args.kind == "redis":
        server_cmd = [
            args.bin,
            "--port",
            str(args.port),
            "--save",
            "",
            "--appendonly",
            "no",
        ]
    else:
        server_cmd = [args.bin, "--bind", "127.0.0.1", "--port", str(args.port)]

    proc = subprocess.Popen(  # nosec B603 - benchmark harness uses trusted local paths.
        server_cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        wait_for_server(args.port, proc)
        preload_keys(args.port, args.keyspace, args.value_size)
        mget_keys = ["key:__rand_int__"] * 10
        subprocess.run(  # nosec B603 - benchmark harness uses trusted local paths.
            [
                args.redis_benchmark,
                "-h",
                "127.0.0.1",
                "-p",
                str(args.port),
                "-r",
                str(args.keyspace),
                "-n",
                str(args.requests),
                "-c",
                str(args.clients),
                "-P",
                str(args.pipeline),
                "-q",
                "mget",
                *mget_keys,
            ],
            check=True,
            timeout=60,
        )
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
