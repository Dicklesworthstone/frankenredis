#!/usr/bin/env python3
"""Run one resp_workload.py mode against a fresh FrankenRedis server binary."""

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
    parser.add_argument("--mode", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--requests", type=int, default=300_000)
    parser.add_argument("--clients", type=int, default=50)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--keyspace", type=int, default=10_000)
    parser.add_argument("--datasize", type=int, default=3)
    parser.add_argument("--key-prefix", default="fr6tsou")
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
        bench = subprocess.run(
            [
                sys.executable,
                str(here / "pass1" / "resp_workload.py"),
                "--port",
                str(args.port),
                "--mode",
                args.mode,
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
                "--json-out",
                args.json_out,
            ],
            check=False,
            text=True,
        )
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
