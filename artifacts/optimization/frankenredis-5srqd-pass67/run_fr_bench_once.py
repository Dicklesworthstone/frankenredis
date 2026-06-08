#!/usr/bin/env python3
"""Run one fr-bench sample against a fresh FrankenRedis server."""

import argparse
import socket
import subprocess
import sys
import time
from pathlib import Path


def wait_for_port(port: int, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise RuntimeError(f"server did not open port {port}: {last_error}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--bench-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--workload", default="get")
    parser.add_argument("--requests", type=int, default=300_000)
    parser.add_argument("--clients", type=int, default=50)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--keyspace", type=int, default=10_000)
    parser.add_argument("--datasize", type=int, default=3)
    parser.add_argument("--key-prefix", default="fr:5srqd")
    parser.add_argument("--json-out")
    args = parser.parse_args()

    here = Path(__file__).resolve().parent
    server = subprocess.Popen(
        [args.server_bin, "--bind", "127.0.0.1", "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        cwd=here,
        text=True,
    )
    try:
        wait_for_port(args.port, 5.0)
        command = [
            args.bench_bin,
            "--host",
            "127.0.0.1",
            "--port",
            str(args.port),
            "--workload",
            args.workload,
            "--requests",
            str(args.requests),
            "--clients",
            str(args.clients),
            "--pipeline",
            str(args.pipeline),
            "--keyspace",
            str(args.keyspace),
            "--datasize",
            str(args.datasize),
            "--key-prefix",
            args.key_prefix,
        ]
        if args.json_out:
            command.extend(["--json-out", args.json_out])
        bench = subprocess.run(command, check=False, text=True)
        return bench.returncode
    finally:
        server.terminate()
        try:
            server.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=3.0)
        if server.returncode not in (0, -15) and server.stderr is not None:
            err = server.stderr.read()
            if err:
                print(err, file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
