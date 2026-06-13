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


def send_set(port: int, size: int) -> None:
    payload = resp_command(b"SET", b"bigstr", b"x" * size)
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        sock.sendall(payload)
        sock.shutdown(socket.SHUT_WR)
        reply = sock.recv(1024)
    if reply != b"+OK\r\n":
        raise RuntimeError(f"unexpected SET reply: {reply!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--log", required=True)
    parser.add_argument("--requests", default=1000, type=int)
    parser.add_argument("--size", default=1_048_576, type=int)
    parser.add_argument(
        "--redis-benchmark",
        default="/dp/frankenredis/legacy_redis_code/redis/src/redis-benchmark",
    )
    args = parser.parse_args()

    Path(args.log).parent.mkdir(parents=True, exist_ok=True)
    proc = subprocess.Popen(  # nosec B603 - benchmark harness uses trusted local paths.
        [args.bin, "--bind", "127.0.0.1", "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    try:
        wait_for_server(args.port, proc)
        send_set(args.port, args.size)
        subprocess.run(  # nosec B603 - benchmark harness uses trusted local paths.
            [
                args.redis_benchmark,
                "-h",
                "127.0.0.1",
                "-p",
                str(args.port),
                "-n",
                str(args.requests),
                "-c",
                "1",
                "-P",
                "1",
                "-q",
                "getrange",
                "bigstr",
                "0",
                "-1",
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            timeout=30,
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
