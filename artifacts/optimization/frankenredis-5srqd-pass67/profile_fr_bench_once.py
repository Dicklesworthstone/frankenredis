#!/usr/bin/env python3
"""Profile a fresh FrankenRedis server while fr-bench drives a workload."""

import argparse
import os
import signal
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


def stop_process(proc: subprocess.Popen[str], sig: signal.Signals = signal.SIGTERM) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(sig)
    try:
        proc.wait(timeout=5.0)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5.0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--bench-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--perf-data", required=True)
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--workload", default="get")
    parser.add_argument("--requests", type=int, default=1_000_000)
    parser.add_argument("--clients", type=int, default=50)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--keyspace", type=int, default=10_000)
    parser.add_argument("--datasize", type=int, default=3)
    parser.add_argument("--key-prefix", default="fr:5srqd:profile")
    parser.add_argument("--frequency", type=int, default=499)
    args = parser.parse_args()

    here = Path(__file__).resolve().parent
    server = subprocess.Popen(
        [args.server_bin, "--bind", "127.0.0.1", "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        cwd=here,
        text=True,
    )
    perf: subprocess.Popen[str] | None = None
    try:
        wait_for_port(args.port, 5.0)
        perf = subprocess.Popen(
            [
                "perf",
                "record",
                "-F",
                str(args.frequency),
                "-g",
                "-p",
                str(server.pid),
                "-o",
                args.perf_data,
                "--",
                "sleep",
                "120",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=here,
            text=True,
        )
        time.sleep(0.25)
        bench = subprocess.run(
            [
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
                "--json-out",
                args.json_out,
            ],
            check=False,
            text=True,
        )
        return bench.returncode
    finally:
        if perf is not None:
            stop_process(perf, signal.SIGINT)
            if perf.stderr is not None:
                err = perf.stderr.read()
                if err:
                    print(err, file=sys.stderr)
        stop_process(server)
        if server.returncode not in (0, -15) and server.stderr is not None:
            err = server.stderr.read()
            if err:
                print(err, file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
